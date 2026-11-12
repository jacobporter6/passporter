package dev.passporter.notify

import android.Manifest
import androidx.activity.ComponentActivity
import android.content.pm.PackageManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import dev.passporter.device.DeviceStatus
import org.robolectric.Shadows.shadowOf
import java.io.File
import kotlin.concurrent.thread

@RunWith(RobolectricTestRunner::class)
class NotificationPermissionTest {
    private val controller = Robolectric.buildActivity(ComponentActivity::class.java)
    private val activity = controller.get()
    private var answered = 0
    private val permission = NotificationPermission(activity, onAnswered = { answered++ })

    init {
        controller.setup()
    }

    private fun connect() = File(activity.dataDir, "bridge-origin").writeText("https://bridge.example\n")

    private fun lastRequested(): List<String>? =
        shadowOf(activity).lastRequestedPermission?.requestedPermissions?.toList()

    @Test
    fun itAsksOnceConnected() {
        permission.askIfDue()
        assertNull("not before a bridge is connected", lastRequested())
        connect()
        permission.askIfDue()
        assertEquals(listOf(Manifest.permission.POST_NOTIFICATIONS), lastRequested())
    }

    @Test
    fun itNeverAsksWhenGranted() {
        connect()
        shadowOf(activity).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        permission.askIfDue()
        assertNull(lastRequested())
    }

    /** Other prompts (push's distributor picker) wait while it is up, and hear when it is answered. */
    @Test
    fun itSaysWhileItAsksAndWhenAnswered() {
        connect()
        assertFalse(permission.asking)
        permission.askIfDue()
        assertTrue(permission.asking)
        val request = requireNotNull(shadowOf(activity).lastRequestedPermission)
        @Suppress("DEPRECATION") // How Android delivers it; ComponentActivity hands it to the launcher.
        activity.onRequestPermissionsResult(
            request.requestCode,
            request.requestedPermissions,
            intArrayOf(PackageManager.PERMISSION_DENIED),
        )
        assertFalse(permission.asking)
        assertEquals(1, answered)
    }

    /** On the pairing's own thread, as `DeviceAuth` tells its listeners. */
    private fun pairingChanged(status: DeviceStatus) {
        thread { permission.pairingListener.onPairingChanged(status) }.join()
        shadowOf(activity.mainLooper).idle()
    }

    /** Pairing by typed code involves no resume: the pairing itself asks. */
    @Test
    fun itAsksOncePairedWithoutAResume() {
        connect()
        pairingChanged(DeviceStatus(DEVICE))
        assertEquals(listOf(Manifest.permission.POST_NOTIFICATIONS), lastRequested())
    }

    @Test
    fun forgettingThePairingAsksNothing() {
        connect()
        pairingChanged(DeviceStatus(null))
        assertNull(lastRequested())
    }

    /** Asked on resume already: a pairing after it asks no second time. */
    @Test
    fun aPairingAfterTheResumeAskedAsksNoMore() {
        connect()
        permission.askIfDue()
        val first = shadowOf(activity).lastRequestedPermission
        pairingChanged(DeviceStatus(DEVICE))
        assertSame(first, shadowOf(activity).lastRequestedPermission)
    }

    /** A pairing told after the activity went asks nothing of it. */
    @Test
    fun aPairingAfterTheActivityIsDestroyedAsksNothing() {
        connect()
        controller.pause().stop().destroy()
        pairingChanged(DeviceStatus(DEVICE))
        assertNull(lastRequested())
    }
}
