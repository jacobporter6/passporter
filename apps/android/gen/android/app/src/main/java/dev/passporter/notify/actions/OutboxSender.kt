package dev.passporter.notify.actions

import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import org.json.JSONException
import org.json.JSONObject

/**
 * Sends one [OutboxJob] to the bridge as a `POST /v1/actions` with
 * `origin: "notification"` (design §5; the outbox contract in
 * `protocol::result`).
 *
 * A notice carries no `expectedSeq` and no option ids, so the first
 * attempt reads them as the bridge's own car replies do: the agent from
 * `GET /v1/agents`, and for a dialog answer `GET /v1/agents/:id/pending`,
 * whose id must still be the notice's (else [NotSent.ALREADY_ANSWERED]). A
 * reply goes only while the agent is still ready and has not changed state
 * since the person replied (else [NotSent.MOVED_ON]); that is the stale
 * guard's "since you decided" for a reply, which names no dialog. `idle`
 * passes whenever it began: herdr turns `done` into `idle` when the pane is
 * looked at, which is no reason to drop a reply (docs/security.md, D20). The body
 * is then frozen ([FrozenBodies]) and every retry resends it as is, so a
 * lost answer is replayed by the bridge, never decided twice.
 *
 * Nothing is ever resent as something else: a 409 or a refusal is final.
 * Every attempt first checks that the app is still paired as the job's
 * device ([paired]); else nothing is sent ([NotSent.PAIRING_CHANGED]).
 */
class OutboxSender(
    private val bridge: Bridge,
    private val frozen: FrozenBodies,
    /** The device the app is paired as now ([pairedDevice]). */
    private val paired: () -> DeviceId?,
) {
    /**
     * One attempt at [job]. Never throws. The frozen body is forgotten once
     * the outcome is final, unless [stopped] says WorkManager stopped the
     * attempt meanwhile: it runs the job again, which must resend that body.
     */
    fun send(job: OutboxJob, stopped: () -> Boolean = { false }): SendOutcome {
        val outcome = try {
            attempt(job)
        } catch (e: DeviceAuthException) {
            failure(job, e)
        }
        if (outcome !is SendOutcome.Retry && !stopped()) frozen.remove(job.key)
        return outcome
    }

    private fun attempt(job: OutboxJob): SendOutcome {
        if (job.device != paired()) return SendOutcome.Final(changed(job))
        val body = frozen.get(job.key) ?: when (val prepared = prepare(job)) {
            is Prepared.Body -> prepared.json.also { frozen.put(job.key, it) }
            is Prepared.Not -> return prepared.outcome
        }
        return answer(job, bridge.send(Method.POST, ACTIONS, body, job.device))
    }

    private sealed interface Prepared {
        data class Body(val json: String) : Prepared

        data class Not(val outcome: SendOutcome) : Prepared
    }

    private fun prepare(job: OutboxJob): Prepared {
        val request = job.request
        val listed = bridge.send(Method.GET, AGENTS, null, job.device)
        if (!listed.isSuccess) return Prepared.Not(unanswered(listed))
        val agents = BridgeState.agents(listed.body) ?: return Prepared.Not(SendOutcome.Final(NotSent.FAILED))
        val agent = agents.firstOrNull { it.id == request.agent }
            ?: return Prepared.Not(SendOutcome.Final(NotSent.AGENT_GONE))
        val kind = if (request.action == NoticeAction.REPLY) {
            val changed = agent.status == AgentStatus.DONE && agent.statusSince > job.createdAt
            if (!agent.status.isReady || changed) {
                return Prepared.Not(SendOutcome.Final(NotSent.MOVED_ON))
            }
            ActionBody.Kind.Prompt(requireNotNull(job.text))
        } else {
            when (val chosen = choose(job)) {
                is Choice.Option -> chosen.kind
                is Choice.Not -> return Prepared.Not(chosen.outcome)
            }
        }
        return Prepared.Body(ActionBody.json(job, agent.seq, kind))
    }

    private sealed interface Choice {
        data class Option(val kind: ActionBody.Kind.Choose) : Choice

        data class Not(val outcome: SendOutcome) : Choice
    }

    /** The option of the current dialog that [job] means, or why there is none. */
    private fun choose(job: OutboxJob): Choice {
        val pending = requireNotNull(job.request.pending)
        val read = bridge.send(Method.GET, "/v1/agents/${job.request.agent.value}/pending", null, job.device)
        if (read.status == 404) return Choice.Not(SendOutcome.Final(NotSent.AGENT_GONE))
        if (!read.isSuccess) return Choice.Not(unanswered(read))
        val dialog = when (val parsed = BridgeState.dialog(read.body)) {
            null -> return Choice.Not(SendOutcome.Final(NotSent.FAILED))
            BridgeState.Dialog.None -> null
            is BridgeState.Dialog.Open -> parsed.state.takeIf { it.id == pending }
        } ?: return Choice.Not(SendOutcome.Final(NotSent.ALREADY_ANSWERED))
        val note = job.text
        val option = when (job.request.action) {
            NoticeAction.ALLOW_ONCE -> dialog.lowRisk(ALLOW)
            NoticeAction.DENY -> dialog.lowRisk(DENY) ?: dialog.lowRisk(DENY_WITH_FEEDBACK)
            NoticeAction.DENY_WITH_NOTE -> dialog.lowRisk(DENY_WITH_FEEDBACK)?.takeIf { it.acceptsFeedback }
            NoticeAction.REPLY -> null
        } ?: return Choice.Not(SendOutcome.Final(NotSent.NEEDS_APP))
        return Choice.Option(ActionBody.Kind.Choose(pending, option.id, note))
    }

    /** The outcome of the bridge's answer to the action itself. */
    private fun answer(job: OutboxJob, response: HttpResponse): SendOutcome =
        when {
            // Final for the key whatever the body says (the outbox contract).
            response.status == 409 -> SendOutcome.Final(stale(job, response.body))
            response.isSuccess -> if (status(response.body) == "sent") {
                SendOutcome.Sent
            } else {
                // `failed`, a status this app predates, or no readable body:
                // final, as delivery is uncertain.
                SendOutcome.Final(NotSent.UNCERTAIN)
            }
            // The dialog went between the read and the action.
            response.status == 422 && refusal(response.body) in GONE_DIALOG -> SendOutcome.Final(NotSent.ALREADY_ANSWERED)
            response.status == 403 || response.status == 400 || response.status == 422 ->
                SendOutcome.Final(NotSent.NEEDS_APP)
            response.status == 404 -> SendOutcome.Final(NotSent.AGENT_GONE)
            else -> unanswered(response)
        }

    /**
     * A status that decided nothing: 429 and 5xx (503 is the bridge saying
     * herdr or its audit log was unavailable) are retried; anything else
     * is final.
     */
    private fun unanswered(response: HttpResponse): SendOutcome =
        when {
            response.status == 429 || response.status in 500..599 -> SendOutcome.Retry(response.retryAfterSecs)
            response.status == 401 -> SendOutcome.Final(NotSent.NOT_PAIRED)
            else -> SendOutcome.Final(NotSent.FAILED)
        }

    /**
     * Why a 409 came: the dialog was answered, unless the bridge says it is
     * still pending (only the agent's seq moved): then, as for a reply,
     * the agent moved on.
     */
    private fun stale(job: OutboxJob, body: String): NotSent {
        val pending = job.request.pending ?: return NotSent.MOVED_ON
        val current = try {
            JSONObject(body).optJSONObject("pending")?.opt("id") as? String
        } catch (_: JSONException) {
            null
        }
        return if (current == pending.value) NotSent.MOVED_ON else NotSent.ALREADY_ANSWERED
    }

    private fun status(body: String): String? = field(body, "status")

    private fun refusal(body: String): String? = field(body, "reason")

    private fun field(body: String, name: String): String? =
        try {
            JSONObject(body).opt(name) as? String
        } catch (_: JSONException) {
            null
        }

    /**
     * Why [job] of another pairing is not sent: [NotSent.PAIRING_CHANGED],
     * or [NotSent.UNCERTAIN] once its body was sent (it may have arrived).
     */
    private fun changed(job: OutboxJob): NotSent =
        if (frozen.get(job.key) != null) NotSent.UNCERTAIN else NotSent.PAIRING_CHANGED

    /** A failure to get an answer at all, from [dev.passporter.device.DeviceAuth]. */
    private fun failure(job: OutboxJob, e: DeviceAuthException): SendOutcome =
        when (e) {
            // Paired anew between the check and the request.
            is DeviceAuthException.NotPaired if job.device != paired() -> SendOutcome.Final(changed(job))
            // Refused: a sign-in the bridge lost (a restart mid-challenge, say).
            is DeviceAuthException.Unreachable, is DeviceAuthException.ServerError, is DeviceAuthException.Refused ->
                SendOutcome.Retry(null)
            is DeviceAuthException.RateLimited -> SendOutcome.Retry(e.retryAfterSecs)
            is DeviceAuthException.NotPaired, is DeviceAuthException.Forgotten, is DeviceAuthException.NoBridge ->
                SendOutcome.Final(NotSent.NOT_PAIRED)
            else -> SendOutcome.Final(NotSent.FAILED)
        }

    private companion object {
        const val AGENTS = "/v1/agents"
        const val ACTIONS = "/v1/actions"

        /** The option ids a hook and the screen parser give a dialog's answers (`pending::parse`). */
        const val ALLOW = "yes"
        const val DENY = "no"
        const val DENY_WITH_FEEDBACK = "no_feedback"

        /** Refusals of a `choose` whose dialog is gone (`protocol::RefusalReason`). */
        val GONE_DIALOG = setOf("no_pending_input", "unknown_option")
    }
}
