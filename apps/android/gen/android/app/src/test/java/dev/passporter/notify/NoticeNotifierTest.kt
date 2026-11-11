package dev.passporter.notify

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import android.content.pm.ShortcutManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

@RunWith(RobolectricTestRunner::class)
class NoticeNotifierTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val manager = context.getSystemService(NotificationManager::class.java)
    private val notifier = NoticeNotifier(context)

    private val blocked =
        """{"ping":"ab","notices":[{"agentId":"w1:p1","name":"widget","state":"blocked",
            "line":"Run `pnpm test`?","pendingId":"screen-2-ab","announce":true}]}"""
    private val finished =
        """{"ping":"cd","notices":[{"agentId":"w1:p2","name":"docs","state":"finished"}]}"""

    @Before
    fun grant() {
        shadowOf(context).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
    }

    private fun posted(agent: String) = shadowOf(manager).getNotification(agent, NoticeNotification.ID)

    @Test
    fun eachAgentGetsItsOwnNotification() {
        notifier.show(blocked)
        notifier.show(finished)
        assertEquals(2, shadowOf(manager).allNotifications.size)
        assertEquals(NoticeNotification.CHANNEL_NEEDS_YOU, posted("w1:p1").channelId)
        assertEquals(NoticeNotification.CHANNEL_FINISHED, posted("w1:p2").channelId)
        val shortcuts = context.getSystemService(ShortcutManager::class.java).dynamicShortcuts
        assertEquals(setOf("agent:w1:p1", "agent:w1:p2"), shortcuts.map { it.id }.toSet())
    }

    @Test
    fun aResentMessageDoesNotPostAgain() {
        notifier.show(blocked)
        val first = posted("w1:p1")
        notifier.show(blocked)
        assertSame("left as it was, so it cannot buzz", first, posted("w1:p1"))
        notifier.show(blocked.replace("screen-2-ab", "screen-3-cd"))
        assertNotSame("a new dialog is news", first, posted("w1:p1"))
    }

    @Test
    fun invalidPayloadsPostNothing() {
        val parse = notifier.show("""{"ping":"ab","notices":[{"agentId":"w1/p1"}]}""")
        assertTrue(parse is NoticeParse.Invalid)
        assertEquals(0, shadowOf(manager).allNotifications.size)
    }

    @Test
    fun withoutThePermissionNothingIsPosted() {
        shadowOf(context).denyPermissions(Manifest.permission.POST_NOTIFICATIONS)
        notifier.show(blocked)
        assertEquals(0, shadowOf(manager).allNotifications.size)
    }
}
