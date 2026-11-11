package dev.passporter.notify.actions

import android.app.Application
import android.app.Notification
import android.app.PendingIntent
import android.content.ComponentName
import androidx.core.app.NotificationCompat
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.notice
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
class NoticeActionsTest {
    private val context: Application = RuntimeEnvironment.getApplication()

    private fun actions(notification: Notification): List<NotificationCompat.Action> =
        (0 until NotificationCompat.getActionCount(notification)).map {
            requireNotNull(NotificationCompat.getAction(notification, it))
        }

    @Test
    fun aDialogsActionsAreTheLowRiskAnswersWithoutSemantics() {
        val notification = NoticeNotification.build(context, notice(), now = 1)
        val actions = actions(notification)
        assertEquals(listOf("Allow once", "Deny", "Deny with note"), actions.map { "${it.title}" })
        for (action in actions) {
            assertEquals(NotificationCompat.Action.SEMANTIC_ACTION_NONE, action.semanticAction)
            assertFalse(action.showsUserInterface)
            assertTrue(action.isAuthenticationRequired)
        }
        assertNull(actions[0].remoteInputs)
        assertNull(actions[1].remoteInputs)
        val note = requireNotNull(actions[2].remoteInputs).single()
        assertEquals(NoticeActions.KEY_TEXT, note.resultKey)
        assertEquals("What should it do instead?", "${note.label}")
        assertFalse(actions[2].allowGeneratedReplies)
    }

    @Test
    fun aFinishedTurnsReplyTakesARemoteInputButLeavesTheSemanticReplyToTheCar() {
        val done = notice(state = NoticeState.FINISHED, pending = null)
        val reply = actions(NoticeNotification.build(context, done, now = 1)).single()
        assertEquals("Reply", "${reply.title}")
        assertEquals(NotificationCompat.Action.SEMANTIC_ACTION_NONE, reply.semanticAction)
        assertTrue(reply.isAuthenticationRequired)
        assertFalse(reply.showsUserInterface)
        assertEquals(NoticeActions.KEY_TEXT, requireNotNull(reply.remoteInputs).single().resultKey)
        assertFalse(reply.allowGeneratedReplies)
    }

    @Test
    fun pendingIntentsAreExplicitBroadcastsImmutableUnlessTheyTakeText() {
        val actions = actions(NoticeNotification.build(context, notice(), now = 1))
        val component = ComponentName(context, NoticeActionReceiver::class.java)
        val expected = listOf(
            "passporter://action/allow/w1%3Ap1/screen-2-ab" to true,
            "passporter://action/deny/w1%3Ap1/screen-2-ab" to true,
            "passporter://action/deny-note/w1%3Ap1/screen-2-ab" to false,
        )
        for ((action, want) in actions.zip(expected)) {
            val shadow = shadowOf(requireNotNull(action.actionIntent))
            assertTrue(shadow.isBroadcast)
            val intent = shadow.savedIntent
            assertEquals(component, intent.component)
            assertEquals(NoticeActions.INTENT_ACTION, intent.action)
            assertEquals(want.first, intent.dataString)
            assertEquals("widget", intent.getStringExtra(NoticeActions.EXTRA_NAME))
            val immutable = shadow.flags and PendingIntent.FLAG_IMMUTABLE != 0
            val mutable = shadow.flags and PendingIntent.FLAG_MUTABLE != 0
            assertEquals(intent.dataString, want.second, immutable)
            assertEquals(intent.dataString, !want.second, mutable)
        }
    }
}
