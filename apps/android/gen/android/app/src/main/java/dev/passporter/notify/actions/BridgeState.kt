package dev.passporter.notify.actions

import dev.passporter.notify.AgentId
import dev.passporter.notify.PendingId
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/** An agent's status (`protocol::AgentStatus`). */
enum class AgentStatus(
    /** The wire spelling. */
    val wire: String,
) {
    /** Waiting for input. */
    IDLE("idle"),

    /** Running a turn. */
    WORKING("working"),

    /** At a dialog. */
    BLOCKED("blocked"),

    /** Finished its turn, not yet seen. */
    DONE("done"),

    /** A status this app predates, or herdr could not tell. */
    UNKNOWN("unknown"),
    ;

    /** Ready for a prompt: herdr `idle` or `done` (design §6.1). */
    val isReady: Boolean get() = this == IDLE || this == DONE

    companion object {
        /** The status spelled [wire], or [UNKNOWN]. */
        fun fromWire(wire: String): AgentStatus = entries.firstOrNull { it != UNKNOWN && it.wire == wire } ?: UNKNOWN
    }
}

/** What the outbox needs of one agent of `GET /v1/agents` (`protocol::Agent`). */
data class AgentState(
    /** Which agent. */
    val id: AgentId,
    /** Its status now. */
    val status: AgentStatus,
    /** herdr's `state_change_seq`: an action's `expectedSeq`. */
    val seq: Long,
    /** When it entered [status] (bridge clock, unix ms). */
    val statusSince: Long,
    /** A finished turn not yet seen. */
    val unread: Boolean,
)

/** What the outbox needs of `GET /v1/agents/:id/pending` (`protocol::PendingInput`). */
data class DialogState(
    /** The pending input's id. */
    val id: PendingId,
    /** Its options. */
    val options: List<DialogOption>,
) {
    /** The low-risk option [id], if offered. */
    fun lowRisk(id: String): DialogOption? = options.firstOrNull { it.id == id && it.lowRisk }
}

/** One option of a [DialogState] (`protocol::PendingOption`). */
data class DialogOption(
    /** The option id, e.g. `yes` or `no_feedback`. */
    val id: String,
    /** `risk: "low"`; anything else counts as elevated. */
    val lowRisk: Boolean,
    /** Whether a `choose` of it may carry `feedbackText`. */
    val acceptsFeedback: Boolean,
)

/**
 * Reads the bridge's answers for the outbox, tolerant as the protocol is
 * for outbound types: unknown fields are ignored, and an agent or option
 * this app cannot read is skipped rather than failing the whole answer.
 * Returns `null` only when the answer is not the expected shape at all.
 */
object BridgeState {
    /** The agents of a `GET /v1/agents` body. */
    fun agents(body: String): List<AgentState>? {
        val array = try {
            JSONArray(body)
        } catch (_: JSONException) {
            return null
        }
        return (0 until array.length()).mapNotNull { index -> (array.opt(index) as? JSONObject)?.let(::agent) }
    }

    /**
     * The dialog of a `GET /v1/agents/:id/pending` body: [Dialog.None] for
     * `null` (nothing pending), `null` for something unreadable.
     */
    fun dialog(body: String): Dialog? {
        if (body.trim() == "null") return Dialog.None
        val json = try {
            JSONObject(body)
        } catch (_: JSONException) {
            return null
        }
        val id = (json.opt("id") as? String)?.let(PendingId::parse) ?: return null
        val raw = json.opt("options") as? JSONArray
        val options = (0 until (raw?.length() ?: 0)).mapNotNull { index ->
            (raw?.opt(index) as? JSONObject)?.let(::option)
        }
        return Dialog.Open(DialogState(id, options))
    }

    /** What is pending. */
    sealed interface Dialog {
        /** Nothing. */
        data object None : Dialog

        /** A dialog. */
        data class Open(
            /** The dialog. */
            val state: DialogState,
        ) : Dialog
    }

    private fun agent(json: JSONObject): AgentState? {
        val id = (json.opt("id") as? String)?.let(AgentId::parse) ?: return null
        val status = json.opt("status") as? String ?: return null
        val seq = (json.opt("seq") as? Number)?.toLong() ?: return null
        val since = (json.opt("statusSince") as? Number)?.toLong() ?: return null
        return AgentState(id, AgentStatus.fromWire(status), seq, since, json.opt("unread") == true)
    }

    private fun option(json: JSONObject): DialogOption? {
        val id = json.opt("id") as? String ?: return null
        return DialogOption(id, json.opt("risk") == "low", json.opt("acceptsFeedback") == true)
    }
}
