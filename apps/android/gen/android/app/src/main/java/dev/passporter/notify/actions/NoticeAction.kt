package dev.passporter.notify.actions

import android.net.Uri
import dev.passporter.notify.AgentId
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.PendingId

/**
 * What a notification can be answered with from the shade (design §6.2,
 * §6.6): the PWA's chips for a dialog, minus everything elevated, and a
 * reply to a finished turn. Each becomes one `POST /v1/actions` with
 * `origin: "notification"` ([OutboxSender]).
 */
enum class NoticeAction(
    /** The spelling in an action's intent data ([ActionRequest]) and in the outbox. */
    val wire: String,
) {
    /**
     * A finished turn: the inline reply (text or voice) goes to the agent
     * as a prompt, while it is still idle.
     */
    REPLY("reply"),

    /** A dialog: its low-risk *Allow once* (`yes`). */
    ALLOW_ONCE("allow"),

    /** A dialog: its *Deny* (`no`, or `no_feedback` without a note). */
    DENY("deny"),

    /**
     * A dialog: *Deny* with a note (`no_feedback`), the inline reply as the
     * feedback: the PWA's "Deny opens dictation" (design §6.2).
     */
    DENY_WITH_NOTE("deny-note"),
    ;

    /** Whether it carries the person's text (a `RemoteInput`). */
    val takesText: Boolean get() = this == REPLY || this == DENY_WITH_NOTE

    /** Whether it answers a dialog, and so needs the notice's pending id. */
    val answersDialog: Boolean get() = this != REPLY

    companion object {
        private const val ELLIPSIS = "…"

        private fun shownWhole(line: String?): Boolean = line != null && !line.endsWith(ELLIPSIS)

        /** The action spelled [wire], or `null`. */
        fun fromWire(wire: String): NoticeAction? = entries.firstOrNull { it.wire == wire }

        /**
         * The actions [notice] offers, in their order on the notification.
         * A dialog is answerable only with the pending id the bridge sent
         * (it names the dialog on screen when the notice went out); without
         * one, or for a state this app predates, the tap that opens the app
         * is the only way. *Always allow* and other elevated options are
         * never offered from the shade, and *Allow once* only when the
         * notice shows what it allows whole: a line cut (by the bridge or
         * this app, ending in `…`) could hide the rest of a command behind
         * a harmless start, and no line shows nothing at all. Denying is
         * always safe.
         */
        fun of(notice: Notice): List<NoticeAction> =
            when (notice.state) {
                NoticeState.BLOCKED -> when {
                    notice.pendingId == null -> emptyList()
                    shownWhole(notice.line) -> listOf(ALLOW_ONCE, DENY, DENY_WITH_NOTE)
                    else -> listOf(DENY, DENY_WITH_NOTE)
                }
                NoticeState.FINISHED -> listOf(REPLY)
                NoticeState.UNKNOWN -> emptyList()
            }
    }
}

/**
 * What one notification action is about: the action, the agent and, for a
 * dialog, its pending id. It travels as the action intent's data,
 * `passporter://action/<action>/<agent>[/<pending>]`, which also keeps each
 * notification's pending intents apart, and which an app holding a mutable
 * pending intent cannot change (only its extras are filled in).
 */
data class ActionRequest(
    /** What to do. */
    val action: NoticeAction,
    /** For which agent. */
    val agent: AgentId,
    /** The dialog answered; present exactly when [NoticeAction.answersDialog]. */
    val pending: PendingId?,
) {
    init {
        require((pending != null) == action.answersDialog) { "a pending id exactly for dialog answers" }
    }

    /** The question it answers, as its notification names it ([NoticeNotification.EXTRA_QUESTION]). */
    val question: String get() = pending?.value ?: NoticeNotification.TURN

    /** The intent data of this request. */
    fun toUri(): Uri =
        Uri.Builder().scheme(SCHEME).authority(HOST)
            .appendPath(action.wire)
            .appendPath(agent.value)
            .apply { pending?.let { appendPath(it.value) } }
            .build()

    companion object {
        private const val SCHEME = "passporter"
        private const val HOST = "action"

        /** The request [uri] names, or `null` for anything else. */
        fun parse(uri: Uri?): ActionRequest? {
            if (uri == null || uri.scheme != SCHEME || uri.host != HOST) return null
            val segments = uri.pathSegments
            val action = segments.getOrNull(0)?.let(NoticeAction::fromWire) ?: return null
            val agent = segments.getOrNull(1)?.let(AgentId::parse) ?: return null
            val expected = if (action.answersDialog) 3 else 2
            if (segments.size != expected) return null
            val pending = segments.getOrNull(2)?.let { PendingId.parse(it) ?: return null }
            return ActionRequest(action, agent, pending)
        }

        /** The request of [action] on [notice], or `null` if the notice cannot take it. */
        fun of(action: NoticeAction, notice: Notice): ActionRequest? {
            val pending = if (action.answersDialog) notice.pendingId ?: return null else null
            return ActionRequest(action, notice.agentId, pending)
        }
    }
}
