package dev.passporter.push

import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import java.io.File
import java.io.IOException
import java.nio.file.Files
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Robolectric for the platform's `org.json` and `Log`. */
@RunWith(RobolectricTestRunner::class)
class PushSyncTest {
    private val dir: File = Files.createTempDirectory("push").toFile()
    private val store = PushStore(File(dir, "push.json"))
    private val bridge = FakeBridge()
    private var unregistered = 0
    private val sync = PushSync(store, bridge) { unregistered++ }

    private val first = registration("https://push.example.org/up/first?up=1")
    private val second = registration("https://push.example.org/up/second?up=1")

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    private fun registration(endpoint: String) =
        checkNotNull(Registration.of(endpoint, TestVector.uaPublic, TestVector.auth))

    /** The bridge's push routes, as this device sees them. */
    private class FakeBridge : PushBridge {
        var device: String? = "dev-1"
        var keystoreFails = false
        var vapid: VapidKey = VapidKey.Known("B" + "A".repeat(86))
        var vapidAsked = 0
        val calls = mutableListOf<String>()
        val answers = ArrayDeque<Int>()
        var failure: DeviceAuthException? = null

        override fun deviceId(): String? {
            if (keystoreFails) throw DeviceAuthException.Keystore(IllegalStateException("busy"))
            return device
        }

        override fun vapidKey(): VapidKey {
            vapidAsked++
            return vapid
        }

        override fun subscribe(json: String) = answer("POST ${JSONObject(json).getString("endpoint")}")

        override fun unsubscribe(json: String) = answer("DELETE ${JSONObject(json).getString("endpoint")}")

        private fun answer(call: String): HttpResponse {
            failure?.let { throw it }
            calls += call
            return HttpResponse(answers.removeFirstOrNull() ?: 201, "")
        }
    }

    @Test
    fun aNewEndpointIsPostedOnce() {
        assertEquals(PushState.Subscribed, sync.onNewEndpoint(first))
        // The distributor repeats it on every registration: nothing to do.
        assertEquals(PushState.Subscribed, sync.onNewEndpoint(first))
        assertEquals(PushState.Subscribed, sync.sync())
        assertEquals(listOf("POST ${first.endpoint}"), bridge.calls)
        assertEquals(Subscribed(first, "dev-1"), store.load().subscribed)
    }

    @Test
    fun aReplacedEndpointIsDroppedThenTheNewOnePosted() {
        sync.onNewEndpoint(first)
        bridge.answers += listOf(204, 201)
        assertEquals(PushState.Subscribed, sync.onNewEndpoint(second))
        assertEquals(
            listOf("POST ${first.endpoint}", "DELETE ${first.endpoint}", "POST ${second.endpoint}"),
            bridge.calls,
        )
    }

    @Test
    fun aNewPairingStartsWithNewKeys() {
        sync.onNewEndpoint(first)
        bridge.device = "dev-2"
        // The old pairing's bridge holds these keys: they must go.
        assertEquals(PushState.NoEndpoint, sync.sync())
        assertEquals(1, unregistered)
        assertEquals(PushRecord(), store.load())
        assertEquals(listOf("POST ${first.endpoint}"), bridge.calls)
        // The next registration, with new keys, is posted as the new device.
        assertEquals(PushState.Subscribed, sync.onNewEndpoint(second))
        assertEquals(Subscribed(second, "dev-2"), store.load().subscribed)
        // The same through readiness, after the next pairing.
        bridge.device = "dev-3"
        assertTrue(sync.readiness() is Readiness.Register)
        assertEquals(2, unregistered)
        assertEquals(PushRecord(), store.load())
    }

    @Test
    fun theBridgesRefusalsAreReportedAndRetried() {
        bridge.answers += 400
        assertTrue(sync.onNewEndpoint(first) is PushState.Refused)
        assertNull(store.load().subscribed)
        bridge.answers += 404
        assertEquals(PushState.BridgeOff, sync.sync())
        bridge.answers += 409
        assertEquals(PushState.Full, sync.sync())
        bridge.answers += 503
        assertEquals(PushState.RetryLater, sync.sync())
        assertEquals(PushState.Subscribed, sync.sync())
        assertEquals(5, bridge.calls.size)
    }

    @Test
    fun anUnreachableBridgeIsTriedAgainLater() {
        bridge.failure = DeviceAuthException.Unreachable(IOException("offline"))
        assertEquals(PushState.RetryLater, sync.onNewEndpoint(first))
        assertEquals("kept for the next try", first, store.load().registration)
        bridge.failure = null
        assertEquals(PushState.Subscribed, sync.sync())
    }

    @Test
    fun unpairedEverythingIsDroppedAndTheDistributorLeft() {
        sync.onNewEndpoint(first)
        bridge.device = null
        assertEquals(Readiness.NotPaired, sync.readiness())
        assertEquals(PushRecord(), store.load())
        assertEquals(1, unregistered)
        // Once only.
        assertEquals(PushState.NotPaired, sync.sync())
        assertEquals(1, unregistered)
        assertEquals(listOf("POST ${first.endpoint}"), bridge.calls)
    }

    @Test
    fun aRevokedDeviceLeavesTheDistributor() {
        sync.onNewEndpoint(first)
        bridge.failure = DeviceAuthException.Forgotten()
        assertEquals(PushState.NotPaired, sync.onNewEndpoint(second))
        assertEquals(PushRecord(), store.load())
        assertEquals(1, unregistered)
    }

    @Test
    fun aKeystoreHiccupChangesNothing() {
        sync.onNewEndpoint(first)
        bridge.keystoreFails = true
        assertEquals(PushState.RetryLater, sync.sync())
        assertEquals(Readiness.Later, sync.readiness())
        assertEquals(0, unregistered)
        assertEquals(Subscribed(first, "dev-1"), store.load().subscribed)
    }

    @Test
    fun theDistributorsUnregistrationDropsTheBridgesCopy() {
        sync.onNewEndpoint(first)
        assertEquals(PushState.NoEndpoint, sync.onUnregistered())
        assertEquals(listOf("POST ${first.endpoint}", "DELETE ${first.endpoint}"), bridge.calls)
        assertEquals(PushRecord(), store.load())
        assertEquals(PushState.NoEndpoint, sync.sync())
        assertEquals("the distributor left already", 0, unregistered)
    }

    @Test
    fun aKeystoreHiccupDuringUnregistrationKeepsWhatTheBridgeHolds() {
        sync.onNewEndpoint(first)
        bridge.keystoreFails = true
        assertEquals(PushState.RetryLater, sync.onUnregistered())
        assertEquals(PushRecord(null, Subscribed(first, "dev-1")), store.load())
        // The next endpoint replaces it at the bridge.
        bridge.keystoreFails = false
        assertEquals(PushState.Subscribed, sync.onNewEndpoint(second))
        assertEquals(
            listOf("POST ${first.endpoint}", "DELETE ${first.endpoint}", "POST ${second.endpoint}"),
            bridge.calls,
        )
    }

    @Test
    fun readinessFollowsTheBridgeAndKeepsItsKey() {
        bridge.vapid = VapidKey.PushOff
        assertEquals(Readiness.BridgeOff, sync.readiness())
        bridge.vapid = VapidKey.Unknown
        assertEquals(Readiness.Later, sync.readiness())
        bridge.vapid = VapidKey.Known("B" + "A".repeat(86))
        assertEquals(Readiness.Register("B" + "A".repeat(86)), sync.readiness())
        // Asked once a key is known: every resume asks readiness.
        assertEquals(Readiness.Register("B" + "A".repeat(86)), sync.readiness())
        assertEquals(3, bridge.vapidAsked)
    }

    @Test
    fun theRecordSurvivesARestartAndAnUnreadableOneIsEmpty() {
        sync.onNewEndpoint(first)
        assertEquals(store.load(), PushStore(File(dir, "push.json")).load())
        File(dir, "push.json").writeText("{not json")
        assertEquals(PushRecord(), store.load())
    }
}
