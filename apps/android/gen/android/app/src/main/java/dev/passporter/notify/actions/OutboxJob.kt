package dev.passporter.notify.actions

import dev.passporter.notify.AgentId
import dev.passporter.notify.PendingId
import java.util.UUID

/**
 * One answer given from the shade, as the native outbox keeps it until the
 * bridge has it (design §6.5): a durable job with an idempotency key made
 * once, when the person acted, so every retry is the same decision.
 */
data class OutboxJob(
    /** The action's `idempotencyKey`: a lowercase UUID, fixed for life. */
    val key: String,
    /** What was asked for, about which agent and dialog. */
    val request: ActionRequest,
    /** The reply or the note; present exactly when [NoticeAction.takesText]. */
    val text: String?,
    /** The agent's name when the person acted, for the notification. Display only. */
    val name: String,
    /** When the person acted (phone clock, unix ms): the action's `createdAt`. */
    val createdAt: Long,
) {
    init {
        require((text != null) == request.action.takesText) { "text exactly for text actions" }
    }

    companion object {
        /** A new job for [request], with a fresh key. */
        fun create(request: ActionRequest, text: String?, name: String, now: Long): OutboxJob =
            OutboxJob(UUID.randomUUID().toString(), request, text, name, now)
    }
}

/** Why a tap on a notification action queues nothing. */
enum class Unsendable {
    /** Nothing visible was typed or said. */
    EMPTY,

    /** More than [ReplyText.MAX_BYTES]: the app takes longer texts. */
    TOO_LONG,

    /**
     * The phone is locked. Android 12 and later ask to unlock before an
     * action runs; older ones, a watch or another app holding the pending
     * intent do not, so the receiver checks.
     */
    LOCKED,

    /** An answer to the same question is queued already (a second tap). */
    ALREADY_SENDING,
}

/**
 * The person's reply or note, made fit for the bridge's text rules
 * (`protocol::text`): tabs turned into spaces, Unicode line and paragraph
 * separators into line breaks, other control characters (`\r` included)
 * dropped, and trimmed. A prompt keeps its line breaks; a note
 * (single-line `feedbackText`) gets spaces instead. Text with nothing
 * visible (only joiners, say) is empty. What else the bridge refuses (a misplaced
 * invisible character, say) comes back as a refusal and is shown as such.
 */
object ReplyText {
    /**
     * Most UTF-8 bytes sent from the shade: the bridge's `feedbackText`
     * limit, and well inside what an outbox job may hold.
     */
    const val MAX_BYTES = 4 * 1024

    private const val LINE_SEPARATOR = '\u2028'
    private const val PARAGRAPH_SEPARATOR = '\u2029'

    /** Not whitespace, a format character (joiners) or a variation selector: blank by the bridge's rule. */
    private fun isVisible(char: Char): Boolean =
        !char.isWhitespace() && Character.getType(char) != Character.FORMAT.toInt() && char !in '\uFE00'..'\uFE0F'

    /** [raw] cleaned for [action], or why it cannot be sent. */
    fun clean(raw: CharSequence?, action: NoticeAction): Result {
        val singleLine = action != NoticeAction.REPLY
        val text = buildString {
            raw?.forEach { char ->
                when {
                    char == LINE_SEPARATOR || char == PARAGRAPH_SEPARATOR -> append(if (singleLine) ' ' else '\n')
                    char == '\n' && !singleLine -> append(char)
                    char == '\n' || char == '\t' -> append(' ')
                    Character.isISOControl(char) -> Unit
                    else -> append(char)
                }
            }
        }.trim()
        return when {
            text.none(::isVisible) -> Result.Problem(Unsendable.EMPTY)
            text.toByteArray(Charsets.UTF_8).size > MAX_BYTES -> Result.Problem(Unsendable.TOO_LONG)
            else -> Result.Clean(text)
        }
    }

    /** The outcome of [clean]. */
    sealed interface Result {
        /** Text to send. */
        data class Clean(
            /** The cleaned text. */
            val text: String,
        ) : Result

        /** Nothing to send. */
        data class Problem(
            /** Why: [Unsendable.EMPTY] or [Unsendable.TOO_LONG]. */
            val problem: Unsendable,
        ) : Result
    }
}
