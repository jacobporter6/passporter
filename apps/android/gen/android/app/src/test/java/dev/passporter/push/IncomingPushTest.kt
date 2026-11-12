package dev.passporter.push

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import dev.passporter.notify.DEVICE
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeNotifier
import dev.passporter.notify.NoticeOutcome
import dev.passporter.notify.OTHER_DEVICE
import dev.passporter.notify.actions.PairedAs
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.unifiedpush.android.connector.data.PushMessage

/** A message from the connector, through to the notification shade. */
@RunWith(RobolectricTestRunner::class)
class IncomingPushTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val manager = context.getSystemService(NotificationManager::class.java)
    private var registered = true
    private val incoming = IncomingPush(NoticeNotifier(context) { DEVICE }::show) { registered }

    private val blocked =
        """{"ping":"0123456789abcdef0123456789abcdef","notices":[{"agentId":"w1:p1","name":"widget",
            "state":"blocked","line":"Run `pnpm test`?","pendingId":"screen-2-ab","announce":true}]}"""

    @Before
    fun grant() {
        shadowOf(context).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
    }

    private fun message(text: String, decrypted: Boolean = true) = PushMessage(text.toByteArray(), decrypted)

    private fun shown() = shadowOf(manager).allNotifications.size

    @Test
    fun aDecryptedNoticeIsShown() {
        assertEquals(Incoming.Shown(NoticeOutcome.Posted(1)), incoming.handle(message(blocked)))
        assertNotNull(shadowOf(manager).getNotification("w1:p1", NoticeNotification.ID))
        // A push service retry: the same ping, never a second alert.
        assertEquals(Incoming.Shown(NoticeOutcome.Duplicate), incoming.handle(message(blocked)))
    }

    @Test
    fun whatTheConnectorCouldNotDecryptIsDropped() {
        // Plaintext from someone without the keys: it must not show.
        assertEquals(Incoming.NotDecrypted, incoming.handle(message(blocked, decrypted = false)))
        assertEquals(0, shown())
    }

    @Test
    fun unregisteredNothingIsShown() {
        // Unpaired since: the registration is gone.
        registered = false
        assertEquals(Incoming.NotRegistered, incoming.handle(message(blocked)))
        assertEquals(0, shown())
    }

    @Test
    fun aBarePingOrGarbageShowsNothingAlarming() {
        val ping = incoming.handle(message("""{"ping":"ffffffffffffffffffffffffffffffff"}"""))
        assertEquals(Incoming.Shown(NoticeOutcome.Posted(0)), ping)
        val garbage = incoming.handle(message("not json"))
        assertTrue(garbage is Incoming.Shown && garbage.outcome is NoticeOutcome.Invalid)
        assertEquals(Incoming.NotText, incoming.handle(PushMessage(byteArrayOf(0xC3.toByte(), 0x28), true)))
        assertEquals(0, shown())
    }

    @Test
    fun onlyTheCurrentPairingsKeysCount() {
        val registration = requireNotNull(Registration.of("https://push.example/e", TestVector.uaPublic, TestVector.auth))
        val mine = PushRecord(registration, owner = DEVICE.value)
        assertTrue(IncomingPush.isCurrent(mine, PairedAs.Device(DEVICE)))
        assertFalse("keys of an older pairing", IncomingPush.isCurrent(mine, PairedAs.Device(OTHER_DEVICE)))
        assertFalse("unpaired", IncomingPush.isCurrent(mine, PairedAs.Nothing))
        assertTrue("a busy Keystore cannot tell: shown", IncomingPush.isCurrent(mine, PairedAs.Unknown))
        assertFalse("no registration", IncomingPush.isCurrent(PushRecord(), PairedAs.Device(DEVICE)))
    }
}
