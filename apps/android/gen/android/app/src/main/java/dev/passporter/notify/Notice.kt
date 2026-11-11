package dev.passporter.notify

/**
 * The decrypted plaintext of a push message to a `notice` subscription:
 * the bridge's `protocol::PushMessage` (design §6.6).
 *
 * A message is a delta, not a snapshot: it carries the agents with news
 * since the device's last message, so an agent missing from it has simply
 * nothing new.
 */
data class NoticeMessage(
    /** 128 random bits as hex, fresh per message; meaningless. */
    val ping: String,
    /** At most [Notice.MAX_NOTICES] notices, oldest first; none for a bare ping. */
    val notices: List<Notice>,
)

/**
 * One agent's news: the bridge's `protocol::Notice`. Every text is
 * untrusted agent output, already cleaned and bounded by the bridge and
 * again by [NoticeParser]; show it as plain text only.
 */
data class Notice(
    /** Which agent; the conversation to post into. */
    val agentId: AgentId,
    /** The agent's name (its card title), at most [MAX_NAME_CHARS]. */
    val name: String,
    /** Why it is news. */
    val state: NoticeState,
    /** What a blocked agent asks, or a finished turn's headline. */
    val line: String?,
    /** The finished turn's TL;DR *Decide* line: what it needs from you. */
    val decide: String?,
    /** The blocked agent's pending input, for a reply to answer. */
    val pendingId: PendingId?,
    /** Whether Android Auto should announce it (used by the car task). */
    val announce: Boolean,
) {
    companion object {
        /** Most notices in one message (`protocol::MAX_NOTICES`). */
        const val MAX_NOTICES = 4

        /** Most characters of a name (the bridge's `MAX_NAME_CHARS`). */
        const val MAX_NAME_CHARS = 60

        /** Most characters of a line or *Decide* line (`MAX_LINE_CHARS`). */
        const val MAX_LINE_CHARS = 160
    }
}

/** Why a [Notice] was sent: `protocol::NoticeState`. */
enum class NoticeState(
    /** The wire spelling. */
    val wire: String,
) {
    /** The agent is at a dialog and waits for an answer. */
    BLOCKED("blocked"),

    /** The agent finished its turn. */
    FINISHED("finished"),

    /** A state this app predates (serde's `#[serde(other)]`). */
    UNKNOWN("unknown"),
    ;

    companion object {
        /** The state spelled [wire], or [UNKNOWN] for one this app predates. */
        fun fromWire(wire: String): NoticeState =
            entries.firstOrNull { it != UNKNOWN && it.wire == wire } ?: UNKNOWN
    }
}

/**
 * An agent's id: `<pane>` or `<pane>~<session>`, each part in the bridge's
 * identifier charset (`protocol::AgentId`). URL-path safe.
 */
@JvmInline
value class AgentId private constructor(
    /** The wire form. */
    val value: String,
) {
    override fun toString(): String = value

    companion object {
        /** The id spelled [raw], or `null` if the bridge would refuse it. */
        fun parse(raw: String): AgentId? {
            val parts = raw.split(SEPARATOR)
            val valid = parts.size <= 2 && parts.all(::isIdent)
            return if (valid) AgentId(raw) else null
        }

        private const val SEPARATOR = '~'
    }
}

/** A dialog's id, opaque to the app (`protocol::PendingId`). */
@JvmInline
value class PendingId private constructor(
    /** The wire form. */
    val value: String,
) {
    override fun toString(): String = value

    companion object {
        /** The id spelled [raw], or `null` if the bridge would refuse it. */
        fun parse(raw: String): PendingId? = if (isIdent(raw)) PendingId(raw) else null
    }
}

/** Most bytes of an identifier part (`protocol::MAX_ID_LEN`). */
private const val MAX_ID_LEN = 128

/**
 * The bridge's identifier charset: 1..=[MAX_ID_LEN] ASCII letters, digits,
 * `:`, `.`, `_` and `-`, not made of dots alone.
 */
private fun isIdent(value: String): Boolean =
    value.length in 1..MAX_ID_LEN &&
        value.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' || it in ":._-" } &&
        !value.all { it == '.' }
