package dev.passporter.notify.actions

import dev.passporter.notify.DEVICE
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.notify.OTHER_DEVICE
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.io.IOException

@RunWith(RobolectricTestRunner::class)
class OutboxSenderTest {
    private val frozen = MemoryFrozen()

    private fun send(bridge: Bridge, job: OutboxJob) = OutboxSender(bridge, frozen) { PairedAs.Device(DEVICE) }.send(job)

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
            // Done again, but since well after the reply: another turn.
            FakeBridge.of(status = "done", since = 5000),
        )) {
            assertEquals(SendOutcome.Final(NotSent.MOVED_ON), send(bridge, job(NoticeAction.REPLY, text = "go")))
            assertEquals(emptyList<String>(), bridge.actions)
        }
    }

    /** The reply's time (phone clock, 2000) and the agent's (bridge clock) are compared on the bridge's clock. */
    private fun replyWithClocks(since: Long, bridgeAhead: Long, dateTruncation: Long = 0): SendOutcome {
        val phoneNow = 60_000L
        val bridge = FakeBridge.of(status = "done", dialog = "null", since = since, date = phoneNow + bridgeAhead - dateTruncation)
        return OutboxSender(bridge, frozen, now = { phoneNow }) { PairedAs.Device(DEVICE) }.send(job(NoticeAction.REPLY, text = "go"))
    }

    @Test
    fun aPhoneClockBehindTheBridgesNeverCallsAFreshReplyStale() {
        val hour = 3_600_000L
        // Done 5 s before the reply, on the bridge's clock an hour ahead.
        assertEquals(SendOutcome.Sent, replyWithClocks(since = 2000 + hour - 5000, bridgeAhead = hour))
        // Done just before the reply, with `Date` cut to the second below.
        assertEquals(SendOutcome.Sent, replyWithClocks(since = 2000 + hour - 100, bridgeAhead = hour, dateTruncation = 999))
        // Another turn done 5 s after the reply is still caught.
        assertEquals(SendOutcome.Final(NotSent.MOVED_ON), replyWithClocks(since = 2000 + hour + 5000, bridgeAhead = hour))
    }

    @Test
    fun aPhoneClockAheadOfTheBridgesNeverLetsAReplyIntoALaterTurn() {
        val hour = 3_600_000L
        // Another turn done 5 s after the reply, on the bridge's clock an hour behind.
        assertEquals(SendOutcome.Final(NotSent.MOVED_ON), replyWithClocks(since = 2000 - hour + 5000, bridgeAhead = -hour))
        assertEquals(SendOutcome.Sent, replyWithClocks(since = 2000 - hour - 5000, bridgeAhead = -hour))
    }

    @Test
    fun withoutADateTheClocksAreTakenAsTheSame() {
        val bridge = FakeBridge.of(status = "done", dialog = "null", since = 3000)
        val sender = OutboxSender(bridge, frozen, now = { 60_000L }) { PairedAs.Device(DEVICE) }
        assertEquals("within the tolerance", SendOutcome.Sent, sender.send(job(NoticeAction.REPLY, text = "go")))
    }

    @Test
    fun aReplyStillGoesAfterThePaneWasSeenOnTheDesktop() {
        // herdr turns `done` into `idle` when the pane is looked at.
        val seen = FakeBridge.of(status = "idle", dialog = "null", since = 2001)
        assertEquals(SendOutcome.Sent, send(seen, job(NoticeAction.REPLY, text = "go")))
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
            HttpResponse(422, """{"status":"refused","reason":"feedback_not_accepted"}""") to SendOutcome.Final(NotSent.NEEDS_APP),
            HttpResponse(422, """{"status":"refused","reason":"unknown_option"}""") to SendOutcome.Final(NotSent.ALREADY_ANSWERED),
            HttpResponse(422, """{"status":"refused","reason":"no_pending_input"}""") to SendOutcome.Final(NotSent.ALREADY_ANSWERED),
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
            DeviceAuthException.Refused() to SendOutcome.Retry(null),
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

    @Test
    fun a409WhileTheDialogIsStillPendingMeansTheAgentMovedOn() {
        val seqOnly = HttpResponse(409, """{"status":"stale","pending":{"id":"screen-2-ab","kind":"permission"}}""")
        assertEquals(SendOutcome.Final(NotSent.MOVED_ON), send(FakeBridge.of(action = seqOnly), job(NoticeAction.ALLOW_ONCE)))
    }

    @Test
    fun aStoppedAttemptKeepsItsFrozenBody() {
        val outcome = OutboxSender(FakeBridge.of(), frozen) { PairedAs.Device(DEVICE) }.send(job(NoticeAction.ALLOW_ONCE)) { true }
        assertEquals(SendOutcome.Sent, outcome)
        assertEquals("the rerun resends it", setOf(KEY), frozen.keys())
    }

    /** Paired anew (or unpaired) since the person answered: nothing reaches the bridge now paired. */
    @Test
    fun anAnswerOfAnotherPairingIsNeverSent() {
        for (paired in listOf(PairedAs.Device(OTHER_DEVICE), PairedAs.Nothing)) {
            val bridge = FakeBridge.of()
            val outcome = OutboxSender(bridge, frozen) { paired }.send(job(NoticeAction.REPLY, "hi"))
            assertEquals(SendOutcome.Final(NotSent.PAIRING_CHANGED), outcome)
            assertTrue("$paired: nothing asked", bridge.requests.isEmpty())
        }
    }

    @Test
    fun everyRequestGoesOnlyAsTheJobsDevice() {
        val bridge = FakeBridge.of()
        send(bridge, job(NoticeAction.ALLOW_ONCE))
        assertEquals(List(3) { DEVICE }, bridge.devices)
    }

    /** Paired anew between the check and the request: `DeviceAuth` refuses the new pairing's token. */
    @Test
    fun aPairingThatChangesMidAttemptStillSendsNothing() {
        var paired: PairedAs = PairedAs.Device(DEVICE)
        val bridge = FakeBridge {
            paired = PairedAs.Device(OTHER_DEVICE)
            throw DeviceAuthException.NotPaired()
        }
        assertEquals(SendOutcome.Final(NotSent.PAIRING_CHANGED), OutboxSender(bridge, frozen) { paired }.send(job(NoticeAction.REPLY, "hi")))
    }

    /** Even with a frozen body: it belongs to the old pairing, whose conversation is never brought back. */
    @Test
    fun anAnswerOfAnotherPairingWithAFrozenBodyIsDroppedAndItsBodyForgotten() {
        frozen.put(KEY, "{}")
        val outcome = OutboxSender(FakeBridge.of(), frozen) { PairedAs.Device(OTHER_DEVICE) }.send(job(NoticeAction.REPLY, "hi"))
        assertEquals(SendOutcome.Final(NotSent.PAIRING_CHANGED), outcome)
        assertTrue(frozen.bodies.isEmpty())
    }

    /** `DeviceAuth` refused a token of another pairing: dropped, without asking the Keystore again. */
    @Test
    fun aTokenOfAnotherDeviceEndsAsPairingChangedEvenWhenThePairingCannotBeRead() {
        var paired: PairedAs = PairedAs.Device(DEVICE)
        val bridge = FakeBridge {
            paired = PairedAs.Unknown
            throw DeviceAuthException.OtherDevice()
        }
        assertEquals(SendOutcome.Final(NotSent.PAIRING_CHANGED), OutboxSender(bridge, frozen) { paired }.send(job(NoticeAction.REPLY, "hi")))
    }

    /** Its key deleted mid-sign-in by a forget: not a failure that would bring the old conversation back. */
    @Test
    fun anyFailureAfterThePairingChangedEndsAsPairingChanged() {
        var paired: PairedAs = PairedAs.Device(DEVICE)
        val bridge = FakeBridge {
            paired = PairedAs.Nothing
            throw DeviceAuthException.Keystore(IllegalStateException("key gone"))
        }
        assertEquals(SendOutcome.Final(NotSent.PAIRING_CHANGED), OutboxSender(bridge, frozen) { paired }.send(job(NoticeAction.REPLY, "hi")))
    }

    /** A busy Keystore after boot cannot tell: the job waits rather than being dropped. */
    @Test
    fun anUnknownPairingIsRetried() {
        val bridge = FakeBridge.of()
        assertEquals(SendOutcome.Retry(null), OutboxSender(bridge, frozen) { PairedAs.Unknown }.send(job(NoticeAction.REPLY, "hi")))
        assertTrue(bridge.requests.isEmpty())
    }
}
