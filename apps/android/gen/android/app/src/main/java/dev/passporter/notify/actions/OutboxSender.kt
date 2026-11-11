package dev.passporter.notify.actions

import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.notify.PendingId
import org.json.JSONException
import org.json.JSONObject

/**
 * An authenticated request to the paired bridge: [dev.passporter.device.DeviceAuth.send]
 * with an app-scope token in the app, a fake in tests. Blocking; throws
 * [DeviceAuthException] when there is no answer or no session.
 */
fun interface Bridge {
    /** Sends [method] [path] with [json] as its body, if any. */
    fun send(method: Method, path: String, json: String?): HttpResponse
}

/**
 * The frozen `POST /v1/actions` body of each job, by idempotency key. Once
 * a body has been sent, every retry sends it again unchanged: the bridge
 * replays its first result for the key (or refuses another body under it).
 */
interface FrozenBodies {
    /** The body frozen for [key], if any. */
    fun get(key: String): String?

    /** Freezes [body] for [key]. */
    fun put(key: String, body: String)

    /** Forgets [key]'s body, once its job is settled. */
    fun remove(key: String)
}

/** Why an answer was not sent, final for its job. */
enum class NotSent {
    /** The dialog was answered (or replaced) before this answer arrived. */
    ALREADY_ANSWERED,

    /** The agent moved on since the person replied (it worked, or asks something). */
    MOVED_ON,

    /** The agent is gone. */
    AGENT_GONE,

    /** The bridge refused it from a notification (step-up, an option the shade cannot pick, invalid text). */
    NEEDS_APP,

    /** The bridge handed it to herdr, which failed: it may or may not have arrived. Never resent. */
    UNCERTAIN,

    /** The app is not (or no longer) paired. */
    NOT_PAIRED,

    /** The bridge stayed unreachable for every retry. */
    UNREACHABLE,

    /** Too long to send from the shade (never reaches the outbox). */
    TOO_LONG,

    /** Anything else: an answer this app cannot read, a broken key store. */
    FAILED,
}

/** What one attempt of a job came to. */
sealed interface SendOutcome {
    /** The bridge sent it to the agent (or had, for a replayed key). */
    data object Sent : SendOutcome

    /** Final: not sent, and never retried. */
    data class Final(
        /** Why. */
        val reason: NotSent,
    ) : SendOutcome

    /** Nothing decided: try the same body again later. */
    data class Retry(
        /** The bridge's `Retry-After`, if it gave one. */
        val afterSecs: Long?,
    ) : SendOutcome
}

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
 * guard's "since you decided" for a reply, which names no dialog. The body
 * is then frozen ([FrozenBodies]) and every retry resends it as is, so a
 * lost answer is replayed by the bridge, never decided twice.
 *
 * Nothing is ever resent as something else: a 409 or a refusal is final.
 */
class OutboxSender(
    private val bridge: Bridge,
    private val frozen: FrozenBodies,
) {
    /** One attempt at [job]. Never throws. */
    fun send(job: OutboxJob): SendOutcome {
        val outcome = try {
            attempt(job)
        } catch (e: DeviceAuthException) {
            failure(e)
        }
        if (outcome !is SendOutcome.Retry) frozen.remove(job.key)
        return outcome
    }

    private fun attempt(job: OutboxJob): SendOutcome {
        val body = frozen.get(job.key) ?: when (val prepared = prepare(job)) {
            is Prepared.Body -> prepared.json.also { frozen.put(job.key, it) }
            is Prepared.Not -> return prepared.outcome
        }
        return answer(job, bridge.send(Method.POST, ACTIONS, body))
    }

    private sealed interface Prepared {
        data class Body(val json: String) : Prepared

        data class Not(val outcome: SendOutcome) : Prepared
    }

    private fun prepare(job: OutboxJob): Prepared {
        val request = job.request
        val listed = bridge.send(Method.GET, AGENTS, null)
        if (!listed.isSuccess) return Prepared.Not(unanswered(listed))
        val agents = BridgeState.agents(listed.body) ?: return Prepared.Not(SendOutcome.Final(NotSent.FAILED))
        val agent = agents.firstOrNull { it.id == request.agent }
            ?: return Prepared.Not(SendOutcome.Final(NotSent.AGENT_GONE))
        val kind = if (request.action == NoticeAction.REPLY) {
            if (!agent.status.isReady || agent.statusSince > job.createdAt) {
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
        val read = bridge.send(Method.GET, "/v1/agents/${job.request.agent.value}/pending", null)
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
            response.status == 409 -> SendOutcome.Final(
                if (job.request.action == NoticeAction.REPLY) NotSent.MOVED_ON else NotSent.ALREADY_ANSWERED,
            )
            response.isSuccess -> if (status(response.body) == "sent") {
                SendOutcome.Sent
            } else {
                // `failed`, a status this app predates, or no readable body:
                // final, as delivery is uncertain.
                SendOutcome.Final(NotSent.UNCERTAIN)
            }
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

    private fun status(body: String): String? =
        try {
            JSONObject(body).opt("status") as? String
        } catch (_: JSONException) {
            null
        }

    /** A failure to get an answer at all, from [dev.passporter.device.DeviceAuth]. */
    private fun failure(e: DeviceAuthException): SendOutcome =
        when (e) {
            is DeviceAuthException.Unreachable, is DeviceAuthException.ServerError -> SendOutcome.Retry(null)
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
    }
}

/** The body of `POST /v1/actions` for an [OutboxJob] (`protocol::Action`). */
object ActionBody {
    /** The verb and payload. */
    sealed interface Kind {
        /** `prompt` with `text`. */
        data class Prompt(
            /** The prompt. */
            val text: String,
        ) : Kind

        /** `choose` of [option] on dialog [pending], with an optional note. */
        data class Choose(
            /** The dialog. */
            val pending: PendingId,
            /** The option id. */
            val option: String,
            /** `feedbackText`, if any. */
            val feedback: String?,
        ) : Kind
    }

    /** The JSON body: fields in the protocol's order, `origin: "notification"`. */
    fun json(job: OutboxJob, seq: Long, kind: Kind): String {
        val (name, payload) = when (kind) {
            is Kind.Prompt -> "prompt" to JSONObject().put("text", kind.text)
            is Kind.Choose -> "choose" to JSONObject()
                .put("pendingId", kind.pending.value)
                .put("optionId", kind.option)
                .apply { kind.feedback?.let { put("feedbackText", it) } }
        }
        return JSONObject()
            .put("idempotencyKey", job.key)
            .put("agentId", job.request.agent.value)
            .put("expectedSeq", seq)
            .put("createdAt", job.createdAt)
            .put("kind", name)
            .put("payload", payload)
            .put("origin", ORIGIN)
            .toString()
    }

    /** Every action from the shade: the bridge's notification policy applies. */
    const val ORIGIN = "notification"
}
