package dev.passporter.notify

import android.app.Application
import android.app.Notification
import android.app.NotificationManager
import androidx.core.app.NotificationCompat
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

@RunWith(RobolectricTestRunner::class)
class NoticeNotificationTest {
    private val context: Application = RuntimeEnvironment.getApplication()

    private fun messages(notification: Notification): List<Pair<String, String>> {
        val style = requireNotNull(
            NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(notification),
        )
        return style.messages.map { "${it.person?.name}" to "${it.text}" }
    }

    @Test
    fun aBlockedAgentIsAConversationOnNeedsYou() {
        val notification = NoticeNotification.build(context, notice(), now = 1000)
        assertEquals(NoticeNotification.CHANNEL_NEEDS_YOU, notification.channelId)
        assertEquals(Notification.CATEGORY_MESSAGE, notification.category)
        assertEquals(listOf("widget" to "Run `pnpm test`?"), messages(notification))
        assertEquals("agent:w1:p1", notification.shortcutId)
        assertEquals("agent:w1:p1", notification.locusId?.id)
        assertEquals(1000L, notification.`when`)
        assertFalse("news sounds again", notification.flags and Notification.FLAG_ONLY_ALERT_ONCE != 0)
        assertTrue(notification.flags and Notification.FLAG_AUTO_CANCEL != 0)
        assertFalse(notification.allowSystemGeneratedContextualActions)
        assertEquals("no actions until the reply task", 0, notification.actions?.size ?: 0)
        assertEquals(Notification.VISIBILITY_PRIVATE, notification.visibility)
        val public = requireNotNull(notification.publicVersion)
        assertEquals("Passporter", public.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        assertEquals("An agent needs you", public.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
    }

    @Test
    fun aFinishedTurnIsQuieterAndShowsItsHeadlineAndDecideLineOnly() {
        val done = notice(
            name = "docs",
            state = NoticeState.FINISHED,
            line = "Fixed the flaky test",
            decide = "Merge PR #12?",
            pending = null,
        )
        val notification = NoticeNotification.build(context, done, now = 1)
        assertEquals(NoticeNotification.CHANNEL_FINISHED, notification.channelId)
        assertEquals(
            listOf("docs" to "Fixed the flaky test", "docs" to "Decide: Merge PR #12?"),
            messages(notification),
        )
        assertEquals(
            "Decide: Merge PR #12?",
            notification.extras.getCharSequence(Notification.EXTRA_TEXT).toString(),
        )
    }

    @Test
    fun withoutALineTheConversationSaysWhyItIsNews() {
        val blocked = NoticeNotification.build(context, notice(line = null), now = 1)
        assertEquals(listOf("widget" to "Waiting for you"), messages(blocked))
        val done = notice(state = NoticeState.FINISHED, line = null, pending = null)
        val finished = NoticeNotification.build(context, done, now = 1)
        assertEquals(listOf("widget" to "Finished its turn"), messages(finished))
    }

    @Test
    fun channelsHaveTheirImportance() {
        NoticeNotification.createChannels(context)
        val manager = context.getSystemService(NotificationManager::class.java)
        assertEquals(
            NotificationManager.IMPORTANCE_HIGH,
            manager.getNotificationChannel(NoticeNotification.CHANNEL_NEEDS_YOU).importance,
        )
        assertEquals(
            NotificationManager.IMPORTANCE_DEFAULT,
            manager.getNotificationChannel(NoticeNotification.CHANNEL_FINISHED).importance,
        )
    }

    @Test
    fun aTapOpensTheAppAtTheAgent() {
        val id = agent("w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4")
        val notification = NoticeNotification.build(context, notice(agent = id.value), now = 1)
        val intent = shadowOf(notification.contentIntent).savedIntent
        assertEquals("dev.passporter.app.MainActivity", intent.component?.className)
        assertEquals(NoticeNotification.ACTION_OPEN_AGENT, intent.action)
        assertEquals(id, NoticeNotification.agentToOpen(intent))
        assertNull(NoticeNotification.agentToOpen(intent.setAction("android.intent.action.MAIN")))
        val other = NoticeNotification.build(context, notice(agent = "w1:p2"), now = 1)
        assertFalse(
            "each agent has its own pending intent",
            shadowOf(other.contentIntent).savedIntent.filterEquals(shadowOf(notification.contentIntent).savedIntent),
        )
    }

    @Test
    fun theShortcutIsTheAgentsConversation() {
        val shortcut = NoticeNotification.shortcut(context, notice())
        assertEquals("agent:w1:p1", shortcut.id)
        assertEquals("widget", shortcut.shortLabel)
        assertEquals("agent:w1:p1", shortcut.locusId?.id)
        assertEquals(NoticeNotification.ACTION_OPEN_AGENT, shortcut.intent.action)
    }
}
