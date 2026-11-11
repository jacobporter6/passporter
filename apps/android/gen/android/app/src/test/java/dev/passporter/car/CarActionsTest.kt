package dev.passporter.car

import android.app.Application
import android.app.Notification
import android.app.PendingIntent
import android.content.ComponentName
import androidx.core.app.NotificationCompat
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.actions.NoticeActionReceiver
import dev.passporter.notify.notice
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

/** Which actions Android Auto is given, per notice: the client side of the car policy. */
@RunWith(RobolectricTestRunner::class)
class CarActionsTest {
    private val context: Application = RuntimeEnvironment.getApplication()

    private fun build(notice: Notice): Notification = NoticeNotification.build(context, notice, now = 1)

    private fun visible(notification: Notification): List<NotificationCompat.Action> =
        (0 until NotificationCompat.getActionCount(notification)).map {
            requireNotNull(NotificationCompat.getAction(notification, it))
        }

    private fun invisible(notification: Notification): List<NotificationCompat.Action> =
        NotificationCompat.getInvisibleActions(notification)

    private val semantic = setOf(
        NotificationCompat.Action.SEMANTIC_ACTION_REPLY,
        NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ,
    )

    /** Every notice shape the bridge can send, announced or not. */
    private val everyNotice = listOf(
        notice(),
        notice(line = "Run `rm -rf build && cargo build --release --target aarch64…`?"),
        notice(pending = null),
        notice(state = NoticeState.FINISHED, pending = null, line = "Fixed it", decide = "Merge now?"),
        notice(state = NoticeState.FINISHED, pending = null, announce = false),
        notice(announce = false),
    )

    @Test
    fun anAnnouncedDialogGetsAnInvisibleReplyAndMarkAsReadForTheCar() {
        val notification = build(notice())
        val (reply, read) = invisible(notification)
        assertEquals(NotificationCompat.Action.SEMANTIC_ACTION_REPLY, reply.semanticAction)
        assertEquals(NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ, read.semanticAction)
        for (action in listOf(reply, read)) {
            assertFalse(action.showsUserInterface)
            // Android Auto fires it with the phone locked in the car.
            assertFalse(action.isAuthenticationRequired)
        }
        val input = requireNotNull(reply.remoteInputs).single()
        assertEquals(CarActions.KEY_UTTERANCE, input.resultKey)
        assertFalse(reply.allowGeneratedReplies)
        assertTrue(read.remoteInputs.isNullOrEmpty())
        assertEquals("the shade keeps its own", 3, visible(notification).size)
    }

    @Test
    fun carIntentsAreExplicitBroadcastsToTheCarReceiverNamingTheDialog() {
        val component = ComponentName(context, CarReplyReceiver::class.java)
        val expected = listOf(
            "passporter://car/reply/w1%3Ap1/dialog/screen-2-ab" to PendingIntent.FLAG_MUTABLE,
            "passporter://car/read/w1%3Ap1/dialog/screen-2-ab" to PendingIntent.FLAG_IMMUTABLE,
        )
        for ((action, want) in invisible(build(notice())).zip(expected)) {
            val shadow = shadowOf(requireNotNull(action.actionIntent))
            assertTrue(shadow.isBroadcast)
            val intent = shadow.savedIntent
            assertEquals(component, intent.component)
            assertEquals(CarActions.INTENT_ACTION, intent.action)
            assertEquals(want.first, intent.dataString)
            assertEquals("widget", intent.getStringExtra(CarActions.EXTRA_NAME))
            assertTrue(intent.dataString, shadow.flags and want.second != 0)
        }
    }

    @Test
    fun aFinishedTurnAndAnUnnamedDialogAreAnnouncedToo() {
        val turn = invisible(build(notice(state = NoticeState.FINISHED, pending = null, decide = "Merge now?")))
        assertEquals(
            listOf("passporter://car/reply/w1%3Ap1/turn", "passporter://car/read/w1%3Ap1/turn"),
            turn.map { shadowOf(requireNotNull(it.actionIntent)).savedIntent.dataString },
        )
        val unnamed = invisible(build(notice(pending = null)))
        assertEquals(
            "passporter://car/reply/w1%3Ap1/blocked",
            shadowOf(requireNotNull(unnamed.first().actionIntent)).savedIntent.dataString,
        )
        assertTrue("nothing for the shade to answer", visible(build(notice(pending = null))).isEmpty())
    }

    @Test
    fun aNoticeTheBridgeDoesNotAnnounceHasNothingAndroidAutoTakes() {
        for (quiet in listOf(notice(announce = false), notice(state = NoticeState.FINISHED, pending = null, announce = false))) {
            val notification = build(quiet)
            assertTrue(invisible(notification).isEmpty())
            assertTrue(
                "no reply or mark-as-read semantics at all",
                visible(notification).none { it.semanticAction in semantic },
            )
        }
    }

    @Test
    fun androidAutoReachesOnlyTheCarReceiverAndNeverAnAllowOrDeny() {
        for (notice in everyNotice) {
            val notification = build(notice)
            // Android Auto finds actions by semantics: only the car's have any.
            assertTrue(visible(notification).all { it.semanticAction == NotificationCompat.Action.SEMANTIC_ACTION_NONE })
            for (action in invisible(notification)) {
                assertTrue(action.semanticAction in semantic)
                val intent = shadowOf(requireNotNull(action.actionIntent)).savedIntent
                assertEquals(CarReplyReceiver::class.java.name, intent.component?.className)
                val request = requireNotNull(CarRequest.parse(intent.data))
                assertEquals(notice.pendingId, request.pending)
            }
            for (action in visible(notification)) {
                val intent = shadowOf(requireNotNull(action.actionIntent)).savedIntent
                assertEquals(NoticeActionReceiver::class.java.name, intent.component?.className)
                assertTrue("the shade's answers ask to unlock", action.isAuthenticationRequired)
            }
        }
    }

    @Test
    fun theCarReadsTheAgentsNameAndItsLinesAndNothingElse() {
        val long = "x".repeat(Notice.MAX_LINE_CHARS)
        val notification = build(
            notice(state = NoticeState.FINISHED, pending = null, name = "n".repeat(Notice.MAX_NAME_CHARS), line = long, decide = long),
        )
        val style = requireNotNull(NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(notification))
        assertFalse(style.isGroupConversation)
        assertEquals(
            listOf("n".repeat(Notice.MAX_NAME_CHARS) to long, "n".repeat(Notice.MAX_NAME_CHARS) to "Decide: $long"),
            style.messages.map { "${it.person?.name}" to "${it.text}" },
        )
    }
}
