package dev.passporter.notify.actions

import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.io.IOException

@RunWith(RobolectricTestRunner::class)
class OutboxSenderTest {
    private val frozen = MemoryFrozen()

    private fun send(bridge: Bridge, job: OutboxJob) = OutboxSender(bridge, frozen).send(job)

    private fun body(kind: String, payload: String) =
        """{"idempotencyKey":"$KEY","agentId":"w1:p1","expectedSeq":7,"createdAt":2000,""" +
            """"kind":"$kind","payload":$payload,"origin":"notification"}"""

    @Test
    fun allowOnceChoosesTheLowRiskYesOfTheSameDialog() {
        val bridge = FakeBridge.of()
        assertEquals(SendOutcome.Sent, send(bridge, job(NoticeAction.ALLOW_ONCE)))
        assertEquals(
            listOf(
                Sent(Method.GET, "/v1/agents", null),
                Sent(Method.GET, "/v1/agents/w1:p1/pending", null),
                Sent(Method.POST, "/v1/actions", body("choose", """{"pendingId":"screen-2-ab","optionId":"yes"}""")),
            ),
            bridge.requests,
        )
        assertTrue("settled: nothing frozen", frozen.bodies.isEmpty())
    }

    @Test
    fun denyChoosesNoOrElseNoFeedbackWithoutANote() {
        val withNo = PERMISSION.replace("\"no_feedback\"", "\"no\"").replace(",\"acceptsFeedback\":true", "")
        val plain = FakeBridge.of(dialog = withNo)
        send(plain, job(NoticeAction.DENY))
        assertEquals(listOf(body("choose", """{"pendingId":"screen-2-ab","optionId":"no"}""")), plain.actions)
        val feedback = FakeBridge.of()
        send(feedback, job(NoticeAction.DENY))
        assertEquals(listOf(body("choose", """{"pendingId":"screen-2-ab","optionId":"no_feedback"}""")), feedback.actions)
    }

    @Test
    fun denyWithNoteSendsTheNoteAsFeedback() {
        val bridge = FakeBridge.of()
        send(bridge, job(NoticeAction.DENY_WITH_NOTE, text = "use \"nextest\" instead"))
        val payload = """{"pendingId":"screen-2-ab","optionId":"no_feedback","feedbackText":"use \"nextest\" instead"}"""
        assertEquals(listOf(body("choose", payload)), bridge.actions)
    }

    @Test
    fun aNoteOnADialogWhoseDenyTakesNoneNeedsTheApp() {
        val withNo = PERMISSION.replace("\"no_feedback\"", "\"no\"").replace(",\"acceptsFeedback\":true", "")
        val bridge = FakeBridge.of(dialog = withNo)
        assertEquals(SendOutcome.Final(NotSent.NEEDS_APP), send(bridge, job(NoticeAction.DENY_WITH_NOTE, text = "no")))
        assertEquals(emptyList<String>(), bridge.actions)
    }

    @Test
    fun onlyElevatedAnswersNeedTheApp() {
        val elevated = PERMISSION.replace("\"risk\":\"low\"", "\"risk\":\"elevated\"")
        val bridge = FakeBridge.of(dialog = elevated)
        assertEquals(SendOutcome.Final(NotSent.NEEDS_APP), send(bridge, job(NoticeAction.ALLOW_ONCE)))
        assertEquals(emptyList<String>(), bridge.actions)
    }

    @Test
    fun anotherDialogOrNoneMeansItWasAlreadyAnswered() {
        val other = FakeBridge.of(dialog = PERMISSION.replace("screen-2-ab", "screen-3-cd"))
        assertEquals(SendOutcome.Final(NotSent.ALREADY_ANSWERED), send(other, job(NoticeAction.ALLOW_ONCE)))
        val none = FakeBridge.of(dialog = "null")
        assertEquals(SendOutcome.Final(NotSent.ALREADY_ANSWERED), send(none, job(NoticeAction.DENY)))
        assertEquals(emptyList<String>(), other.actions + none.actions)
    }

    @Test
    fun aReplyIsAPromptWhileTheAgentIsStillReady() {
        val bridge = FakeBridge.of(status = "done", dialog = "null")
        assertEquals(SendOutcome.Sent, send(bridge, job(NoticeAction.REPLY, text = "go on\nthen merge")))
        assertEquals(
            listOf(Sent(Method.GET, "/v1/agents", null), Sent(Method.POST, "/v1/actions", body("prompt", """{"text":"go on\nthen merge"}"""))),
            bridge.requests,
        )
    }

    @Test
    fun aReplyAfterTheAgentMovedOnIsNotSent() {
        for (bridge in listOf(
            FakeBridge.of(status = "working"),
            FakeBridge.of(status = "blocked"),
            // Ready again, but since after the reply: another turn.
            FakeBridge.of(status = "idle", since = 2001),
        )) {
            assertEquals(SendOutcome.Final(NotSent.MOVED_ON), send(bridge, job(NoticeAction.REPLY, text = "go")))
            assertEquals(emptyList<String>(), bridge.actions)
        }
    }

    @Test
    fun aGoneAgentIsFinal() {
        val bridge = FakeBridge { HttpResponse(200, "[]") }
        assertEquals(SendOutcome.Final(NotSent.AGENT_GONE), send(bridge, job(NoticeAction.ALLOW_ONCE)))
        val vanished = FakeBridge { if (it.path == "/v1/agents") HttpResponse(200, agents()) else HttpResponse(404, "") }
        assertEquals(SendOutcome.Final(NotSent.AGENT_GONE), send(vanished, job(NoticeAction.ALLOW_ONCE)))
    }

    @Test
    fun a409IsFinalAndNeverResent() {
        val stale = HttpResponse(409, """{"status":"stale"}""")
        assertEquals(SendOutcome.Final(NotSent.ALREADY_ANSWERED), send(FakeBridge.of(action = stale), job(NoticeAction.ALLOW_ONCE)))
        val reply = FakeBridge.of(status = "idle", dialog = "null", action = HttpResponse(409, "not json"))
        assertEquals(SendOutcome.Final(NotSent.MOVED_ON), send(reply, job(NoticeAction.REPLY, text = "go")))
        assertEquals(1, reply.actions.size)
        assertTrue(frozen.bodies.isEmpty())
    }

    @Test
    fun answersToTheActionAreClassified() {
        val cases = listOf(
            HttpResponse(200, """{"status":"sent","replayed":true}""") to SendOutcome.Sent,
            HttpResponse(200, """{"status":"failed","message":"timeout"}""") to SendOutcome.Final(NotSent.UNCERTAIN),
            HttpResponse(200, """{"status":"queued"}""") to SendOutcome.Final(NotSent.UNCERTAIN),
            HttpResponse(200, "") to SendOutcome.Final(NotSent.UNCERTAIN),
            HttpResponse(403, """{"status":"refused","reason":"step_up_required"}""") to SendOutcome.Final(NotSent.NEEDS_APP),
            HttpResponse(400, """{"status":"refused","reason":"invalid_action"}""") to SendOutcome.Final(NotSent.NEEDS_APP),
            HttpResponse(422, """{"status":"refused","reason":"unknown_option"}""") to SendOutcome.Final(NotSent.NEEDS_APP),
            HttpResponse(404, """{"status":"refused","reason":"unknown_agent"}""") to SendOutcome.Final(NotSent.AGENT_GONE),
            HttpResponse(401, "") to SendOutcome.Final(NotSent.NOT_PAIRED),
            HttpResponse(429, "", retryAfterSecs = 12) to SendOutcome.Retry(12),
            HttpResponse(503, """{"status":"failed"}""") to SendOutcome.Retry(null),
            HttpResponse(502, "") to SendOutcome.Retry(null),
            HttpResponse(418, "") to SendOutcome.Final(NotSent.FAILED),
        )
        for ((response, expected) in cases) {
            assertEquals("${response.status} ${response.body}", expected, send(FakeBridge.of(action = response), job(NoticeAction.ALLOW_ONCE)))
        }
    }

    @Test
    fun noAnswerOrNoSessionIsClassified() {
        val cases = listOf(
            DeviceAuthException.Unreachable(IOException("offline")) to SendOutcome.Retry(null),
            DeviceAuthException.ServerError(502) to SendOutcome.Retry(null),
            DeviceAuthException.RateLimited(30) to SendOutcome.Retry(30),
            DeviceAuthException.NotPaired() to SendOutcome.Final(NotSent.NOT_PAIRED),
            DeviceAuthException.Forgotten() to SendOutcome.Final(NotSent.NOT_PAIRED),
            DeviceAuthException.NoBridge() to SendOutcome.Final(NotSent.NOT_PAIRED),
            DeviceAuthException.Refused() to SendOutcome.Final(NotSent.FAILED),
            DeviceAuthException.BadResponse("?") to SendOutcome.Final(NotSent.FAILED),
        )
        for ((failure, expected) in cases) {
            assertEquals(failure.code, expected, send(FakeBridge { throw failure }, job(NoticeAction.ALLOW_ONCE)))
        }
    }

    @Test
    fun readsThatFailAreRetriedOrFinal() {
        val busy = FakeBridge { HttpResponse(503, "", retryAfterSecs = 5) }
        assertEquals(SendOutcome.Retry(5), send(busy, job(NoticeAction.ALLOW_ONCE)))
        val herdrDown = FakeBridge { if (it.path == "/v1/agents") HttpResponse(200, agents()) else HttpResponse(502, "") }
        assertEquals(SendOutcome.Retry(null), send(herdrDown, job(NoticeAction.ALLOW_ONCE)))
        val garbage = FakeBridge { HttpResponse(200, "{\"not\":\"a list\"}") }
        assertEquals(SendOutcome.Final(NotSent.FAILED), send(garbage, job(NoticeAction.ALLOW_ONCE)))
        assertTrue("nothing frozen before a body was made", frozen.bodies.isEmpty())
    }

    @Test
    fun aRetryResendsTheFrozenBodyWithoutReadingAgain() {
        var answers = listOf(HttpResponse(503, ""), HttpResponse(200, """{"status":"sent","replayed":true}"""))
        val bridge = FakeBridge { sent ->
            when (sent.path) {
                "/v1/actions" -> answers.first().also { answers = answers.drop(1) }
                "/v1/agents" -> HttpResponse(200, agents())
                else -> HttpResponse(200, PERMISSION)
            }
        }
        assertEquals(SendOutcome.Retry(null), send(bridge, job(NoticeAction.ALLOW_ONCE)))
        val body = body("choose", """{"pendingId":"screen-2-ab","optionId":"yes"}""")
        assertEquals(mapOf(KEY to body), frozen.bodies)
        // The agent moved on meanwhile: the retry still sends the same body,
        // which the bridge replays (or refuses as stale), never a new one.
        assertEquals(SendOutcome.Sent, send(bridge, job(NoticeAction.ALLOW_ONCE)))
        assertEquals(listOf(body, body), bridge.actions)
        assertEquals("two reads, then the action twice", 4, bridge.requests.size)
        assertTrue(frozen.bodies.isEmpty())
    }
}
