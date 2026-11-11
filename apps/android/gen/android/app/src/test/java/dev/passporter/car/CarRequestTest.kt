package dev.passporter.car

import android.net.Uri
import dev.passporter.notify.NoticeState
import dev.passporter.notify.PendingId
import dev.passporter.notify.agent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class CarRequestTest {
    private val pending = requireNotNull(PendingId.parse("screen-2-ab"))

    @Test
    fun requestsSurviveTheirIntentData() {
        val requests = listOf(
            CarRequest(CarAction.REPLY, agent("w1:p1~dev"), CarSubject.Dialog(pending)),
            CarRequest(CarAction.MARK_AS_READ, agent("w1:p1"), CarSubject.Turn),
            CarRequest(CarAction.REPLY, agent("w1:p1"), CarSubject.UnnamedDialog),
        )
        for (request in requests) assertEquals(request, CarRequest.parse(request.toUri()))
    }

    @Test
    fun onlyANamedDialogGivesAPendingIdAndAQuestion() {
        val dialog = CarRequest(CarAction.REPLY, agent("w1:p1"), CarSubject.Dialog(pending))
        assertEquals(pending, dialog.pending)
        assertEquals("screen-2-ab", dialog.question)
        assertEquals(NoticeState.BLOCKED, dialog.state)
        val turn = dialog.copy(subject = CarSubject.Turn)
        assertNull(turn.pending)
        assertEquals("turn", turn.question)
        assertEquals(NoticeState.FINISHED, turn.state)
        val unnamed = dialog.copy(subject = CarSubject.UnnamedDialog)
        assertNull(unnamed.pending)
        assertNull(unnamed.question)
    }

    @Test
    fun anyOtherUriIsNoRequest() {
        for (raw in listOf(
            "passporter://car/allow/w1%3Ap1/dialog/screen-2-ab",
            "passporter://action/reply/w1%3Ap1/turn",
            "passporter://car/reply/w1%3Ap1",
            "passporter://car/reply/w1%3Ap1/dialog",
            "passporter://car/reply/w1%3Ap1/dialog/screen-2-ab/more",
            "passporter://car/reply/w1%3Ap1/turn/screen-2-ab",
            "passporter://car/reply/w1%3Ap1/dialog/bad%20id",
            "passporter://car/reply/..%2F/turn",
            "https://car/reply/w1%3Ap1/turn",
        )) {
            assertNull(raw, CarRequest.parse(Uri.parse(raw)))
        }
        assertNull(CarRequest.parse(null))
    }
}
