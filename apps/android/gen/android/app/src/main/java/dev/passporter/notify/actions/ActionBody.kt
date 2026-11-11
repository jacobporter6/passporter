package dev.passporter.notify.actions

import dev.passporter.notify.PendingId
import org.json.JSONObject

/** The body of `POST /v1/actions` for an [OutboxJob] (`protocol::Action`). */
object ActionBody {
    /** Every action from the shade: the bridge's notification policy applies. */
    const val ORIGIN = "notification"

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
}
