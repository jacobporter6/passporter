package dev.passporter.notify.actions

/** Why an answer was not sent, final for its job. */
enum class NotSent {
    /** The dialog was answered (or replaced) before this answer arrived. */
    ALREADY_ANSWERED,

    /** The agent moved on since the person replied (it worked, or asks something). */
    MOVED_ON,

    /** The agent is gone. */
    AGENT_GONE,

    /**
     * The bridge refused it from a notification: step-up (which the shade
     * cannot give, so final here although the contract allows a retry with
     * proof), an option the shade cannot pick, or invalid text.
     */
    NEEDS_APP,

    /** The bridge handed it to herdr, which failed: it may or may not have arrived. Never resent. */
    UNCERTAIN,

    /** The app is not (or no longer) paired. */
    NOT_PAIRED,

    /**
     * The app was unpaired or paired anew since the person answered
     * ([OutboxJob.device]): the answer belongs to the old pairing, and is
     * never sent to the new one.
     */
    PAIRING_CHANGED,

    /** The bridge stayed unreachable for every retry. */
    UNREACHABLE,

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
