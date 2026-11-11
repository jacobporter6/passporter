package dev.passporter.car

import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.device.Scope
import dev.passporter.notify.PendingId
import dev.passporter.notify.actions.FakeBridge
import dev.passporter.notify.actions.KEY
import dev.passporter.notify.actions.Sent
import dev.passporter.notify.agent
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** A car job of [utterance] about dialog `screen-2-ab` of `w1:p1` (a finished turn when [pending] is null). */
fun carJob(utterance: String = "no, use the other file", pending: String? = "screen-2-ab"): CarJob {
    val subject = pending?.let { CarSubject.Dialog(requireNotNull(PendingId.parse(it))) } ?: CarSubject.Turn
    return CarJob(KEY, CarRequest(CarAction.REPLY, agent("w1:p1"), subject), utterance, "widget")
}

/** A bridge's `CarReply` body. */
fun carReply(speech: String, status: String = "sent"): String =
    """{"speech":${JSONObject.quote(speech)},"result":{"status":"$status","replayed":false}}"""

@RunWith(RobolectricTestRunner::class)
class CarReplySenderTest {
    private fun send(job: CarJob = carJob(), answer: (Sent) -> HttpResponse): Pair<CarOutcome, FakeBridge> {
        val bridge = FakeBridge(answer)
        return CarReplySender(bridge).send(job) to bridge
    }

    @Test
    fun theUtteranceGoesAsHeardWithThePendingIdAndTheKey() {
        val (outcome, bridge) = send { HttpResponse(200, carReply("Denied, and sent your note.")) }
        assertEquals(CarOutcome.Spoken("Denied, and sent your note.", sent = true), outcome)
        val sent = bridge.requests.single()
        assertEquals(Method.POST, sent.method)
        assertEquals("/v1/agents/w1:p1/car-reply", sent.path)
        assertEquals(
            """{"idempotencyKey":"$KEY","utterance":"no, use the other file","pendingId":"screen-2-ab"}""",
            sent.body,
        )
    }

    @Test
    fun aFinishedTurnsReplyNamesNoDialog() {
        val (_, bridge) = send(carJob("yes", pending = null)) { HttpResponse(200, carReply("Sent.")) }
        assertEquals("""{"idempotencyKey":"$KEY","utterance":"yes"}""", bridge.requests.single().body)
    }

    @Test
    fun theCarBridgeAlwaysSignsInWithACarScope() {
        val scopes = mutableListOf<Scope>()
        val bridge = CarBridge { scope, _, _, _ -> scopes += scope; HttpResponse(200, carReply("Sent.")) }
        CarReplySender(bridge).send(carJob())
        assertEquals(listOf(Scope.CAR), scopes)
    }

    @Test
    fun refusalsAndStaleRepliesAreFinalAndSpoken() {
        val ambiguous = "I'm not sure what you meant. Say just yes to allow, or no and what to do instead."
        for (status in listOf(400, 403, 404, 409, 422)) {
            val (outcome, _) = send { HttpResponse(status, carReply(ambiguous, status = "refused")) }
            assertEquals("$status", CarOutcome.Spoken(ambiguous, sent = false), outcome)
        }
    }

    @Test
    fun theSpokenSentenceIsOneCleanBoundedLine() {
        val (outcome, _) = send { HttpResponse(200, carReply("Allowed:\n‮cargo ${"x".repeat(300)}")) }
        val speech = (outcome as CarOutcome.Spoken).speech
        assertEquals(CarReplySender.MAX_SPEECH_CHARS, speech.codePointCount(0, speech.length))
        assertEquals(true, speech.startsWith("Allowed: cargo xxx"))
    }

    @Test
    fun anAnswerWithoutASentenceIsUncertainOrFailed() {
        assertEquals(CarOutcome.Trouble(CarTrouble.UNCERTAIN), send { HttpResponse(200, "<html>") }.first)
        assertEquals(CarOutcome.Trouble(CarTrouble.UNCERTAIN), send { HttpResponse(409, "") }.first)
        assertEquals(CarOutcome.Trouble(CarTrouble.FAILED), send { HttpResponse(403, """{"error":"car_forbidden"}""") }.first)
        assertEquals(CarOutcome.Trouble(CarTrouble.FAILED), send { HttpResponse(418, "") }.first)
        assertEquals(CarOutcome.Trouble(CarTrouble.NOT_PAIRED), send { HttpResponse(401, "") }.first)
    }

    @Test
    fun busyAndUnavailableBridgesAreRetried() {
        val unreachable = carReply("I can't reach the agent right now. Try again in a moment.", status = "failed")
        assertEquals(CarOutcome.Retry(null), send { HttpResponse(503, unreachable) }.first)
        assertEquals(CarOutcome.Retry(5), send { HttpResponse(429, "", retryAfterSecs = 5) }.first)
        assertEquals(CarOutcome.Retry(null), send { HttpResponse(502, "") }.first)
    }

    @Test
    fun signInFailuresAreRetriedOrFinalAsInTheShadesOutbox() {
        assertEquals(CarOutcome.Retry(null), send { throw DeviceAuthException.Unreachable(java.io.IOException("offline")) }.first)
        assertEquals(CarOutcome.Trouble(CarTrouble.NOT_PAIRED), send { throw DeviceAuthException.NotPaired() }.first)
    }
}
