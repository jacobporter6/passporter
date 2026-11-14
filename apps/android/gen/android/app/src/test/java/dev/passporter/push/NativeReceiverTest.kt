package dev.passporter.push

import com.google.crypto.tink.apps.fixed_webpush.WebPushHybridDecrypt
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId
import dev.passporter.device.HttpResponse
import dev.passporter.notify.NoticeOutcome
import dev.passporter.notify.actions.PairedAs
import java.io.ByteArrayInputStream
import java.io.File
import java.nio.file.Files
import java.util.Base64
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.unifiedpush.android.connector.data.PublicKeySet
import org.unifiedpush.android.connector.keys.KeyManager

/**
 * The built-in receiver end to end on the JVM: the bridge's own envelope
 * vector is verified, decrypted with RFC 8291's example keys (the notice
 * vector's receiver) and shown as the device that registered; registration,
 * pairing changes and switching to the external distributor. Robolectric
 * for the platform's `org.json` and `Log`.
 */
@RunWith(RobolectricTestRunner::class)
class NativeReceiverTest {
    private val dir: File = Files.createTempDirectory("native").toFile()
    private val store = NativeStore(File(dir, "native.json"))
    private val bridge = FakeBridge()
    private val keys = FakeKeys()
    private val shown = mutableListOf<Pair<DeviceId, String>>()
    private var pairedNow: PairedAs = PairedAs.Device(device("dev-0000000000000001"))
    private var ids = ArrayDeque(listOf(EnvelopeVector.registration, "00000000-0000-4000-8000-000000000002"))
    private var clockMs = 0L
    private val receiver = NativeReceiver(
        store = store,
        bridge = bridge,
        keys = keys,
        show = { owner, text ->
            shown += owner to text
            NoticeOutcome.Posted(1)
        },
        paired = { pairedNow },
        nowSecs = { EnvelopeVector.validAt },
        newId = { ids.removeFirst() },
        elapsedMs = { clockMs },
    )

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    private fun device(id: String) = checkNotNull(DeviceId.parse(id))

    /** The bridge, as this device sees it. */
    private inner class FakeBridge : NativeBridge {
        var device: DeviceId? = device("dev-0000000000000001")
        val calls = mutableListOf<String>()
        val answers = ArrayDeque<Int>()
        val streams = ArrayDeque<NativeStream>()
        var failure: DeviceAuthException? = null

        override fun deviceId(): DeviceId? = device

        override fun register(json: String, device: DeviceId): HttpResponse {
            failure?.let { throw it }
            val id = JSONObject(json).getString("registrationId")
            check(device == this.device) { "registered as ${device.value}, paired as ${this.device?.value}" }
            calls += "POST $id"
            val status = answers.removeFirstOrNull() ?: 200
            val body = JSONObject()
                .put("version", 1)
                .put("signingKey", EnvelopeVector.signer)
                .put("streamPath", "/v1/push/native/$id/stream")
            return HttpResponse(status, if (status == 200) body.toString() else "")
        }

        override fun unsubscribe(json: String, device: DeviceId): HttpResponse {
            calls += "DELETE ${JSONObject(json).getString("endpoint")} as ${device.value}"
            return HttpResponse(204, "")
        }

        override fun openStream(path: String, device: DeviceId): NativeStream {
            calls += "GET $path as ${device.value}"
            return streams.removeFirstOrNull() ?: NativeStream(503, null) {}
        }
    }

    /** The connector's key manager, with RFC 8291's example receiver as the one key. */
    private class FakeKeys : KeyManager {
        var exists = false
        var generated = 0
        var deleted = 0

        override fun exists(instance: String) = exists && instance == NativeReceiver.KEY_INSTANCE

        override fun generate(instance: String) {
            exists = true
            generated++
        }

        override fun delete(instance: String) {
            exists = false
            deleted++
        }

        override fun getPublicKeySet(instance: String) = if (exists) PublicKeySet(TestVector.uaPublic, TestVector.auth) else null

        override fun decrypt(instance: String, sealed: ByteArray): ByteArray? {
            if (!exists) return null
            val b64 = Base64.getUrlDecoder()
            return WebPushHybridDecrypt.Builder()
                .withAuthSecret(b64.decode(TestVector.auth))
                .withRecipientPublicKey(b64.decode(TestVector.uaPublic))
                .withRecipientPrivateKey(b64.decode(TestVector.uaPrivate))
                .build()
                .decrypt(sealed, null)
        }
    }

    private fun message(id: String, envelope: String = EnvelopeVector.envelope) =
        JSONObject().put("id", id).put("event", "message").put("message", envelope).toString()

    private fun stream(vararg lines: String) =
        NativeStream(200, ByteArrayInputStream(lines.joinToString("\n", postfix = "\n").toByteArray())) {}

    @Test
    fun registersOncePerProcessWithItsOwnKeysAndKeepsTheSigningKey() {
        assertEquals(NativeState.Registered, receiver.register())
        assertEquals(NativeState.Registered, receiver.register())
        assertEquals(listOf("POST ${EnvelopeVector.registration}"), bridge.calls)
        val record = store.load()
        assertEquals("dev-0000000000000001", record.owner)
        assertEquals(EnvelopeVector.registration, record.registrationId)
        assertEquals(EnvelopeVector.signer, record.signingKey)
        assertEquals(1, keys.generated)
    }

    @Test
    fun aBridgeWithoutACompanionLeavesItToTheExternalDistributor() {
        bridge.answers += 404
        assertEquals(NativeState.BridgeHasNone, receiver.register())
        assertFalse(store.load().registered)
    }

    @Test
    fun aRefusedOrUnreachableRegistrationIsNotRegistered() {
        bridge.answers += 409
        assertEquals(NativeState.Refused(409), receiver.register())
        assertEquals("the id is kept", EnvelopeVector.registration, store.load().registrationId)
        assertFalse("not registered: the external transport takes over", store.load().registered)
        bridge.failure = DeviceAuthException.Unreachable(java.io.IOException("offline"))
        assertEquals(NativeState.RetryLater, receiver.register())
        assertFalse(store.load().registered)
    }

    @Test
    fun aNewPairingRegistersAfreshWithNewKeys() {
        receiver.register()
        bridge.device = device("dev-0000000000000002")
        assertEquals(NativeState.Registered, receiver.register())
        assertEquals(1, keys.deleted)
        assertEquals(2, keys.generated)
        assertEquals("dev-0000000000000002", store.load().owner)
        assertEquals("00000000-0000-4000-8000-000000000002", store.load().registrationId)
    }

    @Test
    fun unpairedNothingIsKept() {
        receiver.register()
        bridge.device = null
        assertEquals(NativeState.NotPaired, receiver.register())
        assertEquals(NativeRecord(), store.load())
        assertFalse(keys.exists)
    }

    @Test
    fun choosingTheExternalDistributorDropsTheRegistrationAsItsOwner() {
        receiver.register()
        receiver.choose(Transport.EXTERNAL)
        assertEquals(NativeState.Off, receiver.register())
        assertEquals(
            "DELETE https://native.passporter.invalid/${EnvelopeVector.registration} as dev-0000000000000001",
            bridge.calls.last(),
        )
        assertEquals(NativeRecord(choice = Transport.EXTERNAL), store.load())
        assertFalse(keys.exists)
        // And back.
        receiver.choose(Transport.BUILT_IN)
        assertEquals(NativeState.Registered, receiver.register())
    }

    @Test
    fun aSignedMessageIsShownAsTheDeviceThatRegisteredAndTheCursorAdvances() {
        receiver.register()
        val outcome = receiver.ingest(message("m1"))
        assertEquals(Ingested.Shown(NoticeOutcome.Posted(1)), outcome)
        assertEquals(listOf(device("dev-0000000000000001") to TestVector.plaintext), shown)
        assertEquals("m1", store.load().cursor)
    }

    @Test
    fun aMessageForAnEarlierPairingIsDroppedUnseen() {
        receiver.register()
        pairedNow = PairedAs.Device(device("dev-0000000000000002"))
        assertEquals(Ingested.OtherPairing, receiver.ingest(message("m1")))
        // Unpaired since (a stream still being read while the reset runs): dropped too.
        pairedNow = PairedAs.Nothing
        assertEquals(Ingested.OtherPairing, receiver.ingest(message("m2")))
        // When the app cannot tell, the registration counts.
        pairedNow = PairedAs.Unknown
        check(receiver.ingest(message("m3")) is Ingested.Shown)
        assertEquals(1, shown.size)
    }

    @Test
    fun aForgedOrForeignMessageIsRefusedUnreadButReadPast() {
        receiver.register()
        val forged = EnvelopeVector.with("expiresAt", EnvelopeVector.expiresAt + 1)
        assertEquals(Ingested.Refused(EnvelopeRefusal.FORGED), receiver.ingest(message("m1", forged)))
        assertEquals("m1", store.load().cursor)
        val foreign = EnvelopeVector.with("registrationId", "00000000-0000-4000-8000-000000000009")
        assertEquals(Ingested.Refused(EnvelopeRefusal.OTHER_REGISTRATION), receiver.ingest(message("m2", foreign)))
        assertTrue(shown.isEmpty())
    }

    @Test
    fun theCompanionsOwnEventsAndBadIdsAreIgnored() {
        receiver.register()
        assertEquals(Ingested.Ignored, receiver.ingest("""{"event":"open"}"""))
        assertEquals(Ingested.Ignored, receiver.ingest("""{"event":"keepalive","id":"k1"}"""))
        assertEquals(Ingested.Ignored, receiver.ingest("not json"))
        assertEquals(Ingested.Ignored, receiver.ingest(message("../x")))
        assertNull(store.load().cursor)
    }

    @Test
    fun theStreamResumesAfterItsCursorAndReadsUntilItEnds() {
        receiver.register()
        bridge.streams += stream("""{"event":"open"}""", message("m7"))
        var connected = false
        assertEquals(NativeState.RetryLater, receiver.listen({ true }, {}, { connected = true }))
        assertTrue(connected)
        assertEquals(1, shown.size)
        bridge.streams += stream()
        receiver.listen({ true }, {}, {})
        assertEquals(
            "GET /v1/push/native/${EnvelopeVector.registration}/stream?since=m7 as dev-0000000000000001",
            bridge.calls.last(),
        )
    }

    @Test
    fun aDroppedRegistrationIsMadeAgain() {
        receiver.register()
        bridge.streams += NativeStream(404, null) {}
        assertEquals(NativeState.Registered, receiver.listen({ true }, {}, {}))
        assertEquals("POST ${EnvelopeVector.registration}", bridge.calls.last())
    }

    @Test
    fun envelopesThatKeepFailingTheSignatureRegisterAgainButNotInALoop() {
        receiver.register()
        val forged = message("m1", EnvelopeVector.with("expiresAt", EnvelopeVector.expiresAt + 1))
        bridge.streams += stream(forged, forged, forged, message("m9"))
        assertEquals(NativeState.Registered, receiver.listen({ true }, {}, {}))
        val posts = { bridge.calls.count { it.startsWith("POST") } }
        assertEquals(2, posts())
        assertTrue("the rest waits for the new registration", shown.isEmpty())
        // A local publisher sending forgeries again soon after: read past, no new registration.
        bridge.streams += stream(forged, forged, forged, forged, message("m9"))
        assertEquals(NativeState.RetryLater, receiver.listen({ true }, {}, {}))
        assertEquals(2, posts())
        assertEquals(1, shown.size)
        // Half an hour on, it may register once more.
        clockMs += NativeReceiver.REREGISTER_EVERY_MS
        bridge.streams += stream(forged, forged, forged)
        assertEquals(NativeState.Registered, receiver.listen({ true }, {}, {}))
        assertEquals(3, posts())
    }

    @Test
    fun aNewPairingsRegistrationEndsTheStreamOfTheOldOne() {
        receiver.register()
        var reconnects = 0
        receiver.onRegistrationChanged = { reconnects++ }
        // A resume's refresh registers the new pairing first (before PairingReset runs).
        bridge.device = device("dev-0000000000000002")
        assertEquals(NativeState.Registered, receiver.register())
        assertEquals("the service is told to end the old stream", 1, reconnects)
    }

    @Test
    fun aStreamStopsReadingOnceItsRegistrationIsReplaced() {
        receiver.register()
        bridge.streams += stream(message("m1"), message("m2"))
        var lines = 0
        val outcome = receiver.listen({ true }, {}, {}) { work ->
            work().also {
                lines++
                // The record changes under the open stream (a new pairing registered).
                store.save(store.load().copy(registrationId = "00000000-0000-4000-8000-000000000005"))
            }
        }
        assertEquals(NativeState.RetryLater, outcome)
        assertEquals("the old stream's next line is not read", 1, lines)
    }

    @Test
    fun withoutARegistrationThereIsNothingToListenTo() {
        assertEquals(NativeState.NotPaired, receiver.listen({ true }, {}, {}))
        assertTrue(bridge.calls.isEmpty())
    }
}
