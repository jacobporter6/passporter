package dev.passporter.notify

import android.Manifest
import androidx.activity.ComponentActivity
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import java.io.File

@RunWith(RobolectricTestRunner::class)
class NotificationPermissionTest {
    private val controller = Robolectric.buildActivity(ComponentActivity::class.java)
    private val activity = controller.get()
    private val permission = NotificationPermission(activity)

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
}
