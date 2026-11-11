package dev.passporter.notify.actions

import android.net.Uri
import dev.passporter.notify.NoticeState
import dev.passporter.notify.PendingId
import dev.passporter.notify.agent
import dev.passporter.notify.notice
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class NoticeActionTest {
    @Test
    fun aDialogWithItsPendingIdTakesAllowOnceDenyAndDenyWithNote() {
        assertEquals(
            listOf(NoticeAction.ALLOW_ONCE, NoticeAction.DENY, NoticeAction.DENY_WITH_NOTE),
            NoticeAction.of(notice()),
        )
    }

    @Test
    fun aDialogWithoutPendingIdIsAnsweredInTheAppOnly() {
        assertEquals(emptyList<NoticeAction>(), NoticeAction.of(notice(pending = null)))
    }

    @Test
    fun aFinishedTurnTakesAReplyAndAnUnknownStateNothing() {
        val done = notice(state = NoticeState.FINISHED, pending = null, decide = "Merge?")
        assertEquals(listOf(NoticeAction.REPLY), NoticeAction.of(done))
        assertEquals(emptyList<NoticeAction>(), NoticeAction.of(notice(state = NoticeState.UNKNOWN)))
    }

    @Test
    fun requestsRoundTripThroughTheirUri() {
        val pending = requireNotNull(PendingId.parse("screen-2-ab"))
        val allow = ActionRequest(NoticeAction.ALLOW_ONCE, agent("w1:p1~s-1"), pending)
        assertEquals("passporter://action/allow/w1%3Ap1~s-1/screen-2-ab", allow.toUri().toString())
        assertEquals(allow, ActionRequest.parse(allow.toUri()))
        val reply = ActionRequest(NoticeAction.REPLY, agent("w1:p1"), null)
        assertEquals(reply, ActionRequest.parse(reply.toUri()))
    }

    @Test
    fun anyOtherUriIsNoRequest() {
        for (uri in listOf(
            null,
            "passporter://agent/w1:p1",
            "https://action/allow/w1:p1/p",
            "passporter://action/always/w1:p1/p",
            "passporter://action/allow/w1:p1",
            "passporter://action/reply/w1:p1/p",
            "passporter://action/allow/w1%2Fp1/p",
            "passporter://action/allow/w1:p1/p/extra",
        )) {
            assertNull(uri, ActionRequest.parse(uri?.let(Uri::parse)))
        }
    }

    @Test
    fun aRequestNeedsThePendingIdOnlyForADialog() {
        assertNull(ActionRequest.of(NoticeAction.ALLOW_ONCE, notice(pending = null)))
        assertEquals(
            ActionRequest(NoticeAction.REPLY, agent("w1:p1"), null),
            ActionRequest.of(NoticeAction.REPLY, notice()),
        )
    }

    @Test
    fun allowOnceNeedsTheWholeAskOnShow() {
        val cut = notice(line = "Run `cargo test && cur…")
        assertEquals(listOf(NoticeAction.DENY, NoticeAction.DENY_WITH_NOTE), NoticeAction.of(cut))
        assertEquals(listOf(NoticeAction.DENY, NoticeAction.DENY_WITH_NOTE), NoticeAction.of(notice(line = null)))
    }
}
