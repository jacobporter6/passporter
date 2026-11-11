package dev.passporter.car

import android.net.Uri
import dev.passporter.notify.AgentId
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.PendingId

/** What Android Auto can do with an announced notice: all it is ever given (design §6.6). */
enum class CarAction(
    /** The spelling in the intent data ([CarRequest]). */
    val wire: String,
) {
    /** The spoken reply: the utterance goes, as heard, to `POST /v1/agents/:id/car-reply`. */
    REPLY("reply"),

    /** The person heard or opened the conversation in the car: local only. */
    MARK_AS_READ("read"),
    ;

    companion object {
        /** The action spelled [wire], or `null`. */
        fun fromWire(wire: String): CarAction? = entries.firstOrNull { it.wire == wire }
    }
}

/** What an announced notice was about, as the car heard it. */
sealed interface CarSubject {
    /** A blocked agent's dialog, named by the pending id the notice carried. */
    data class Dialog(
        /** The dialog the car read aloud. */
        val pending: PendingId,
    ) : CarSubject

    /**
     * A blocked agent whose dialog the bridge could not name: the car can
     * announce it, and the bridge refuses any answer to it aloud.
     */
    data object UnnamedDialog : CarSubject

    /** A finished turn (and its *Decide* line, if any). */
    data object Turn : CarSubject

    companion object {
        /** What [notice] is about, or `null` for a state this app predates. */
        fun of(notice: Notice): CarSubject? =
            when (notice.state) {
                NoticeState.BLOCKED -> notice.pendingId?.let(::Dialog) ?: UnnamedDialog
                NoticeState.FINISHED -> Turn
                NoticeState.UNKNOWN -> null
            }
    }
}

/**
 * What one car action is about: the action, the agent and the
 * [CarSubject]. It travels as the intent's data,
 * `passporter://car/<action>/<agent>/dialog/<pending>`, `…/blocked` or
 * `…/turn`, which keeps each conversation's pending intents apart and
 * which the car (or anything else holding the mutable reply intent)
 * cannot change: only the `RemoteInput` result is filled in.
 */
data class CarRequest(
    /** What to do. */
    val action: CarAction,
    /** For which agent. */
    val agent: AgentId,
    /** About what. */
    val subject: CarSubject,
) {
    /** The pending id a reply names (`pendingId`): only a named dialog's. */
    val pending: PendingId? get() = (subject as? CarSubject.Dialog)?.pending

    /**
     * The question its notification names ([NoticeNotification.question]),
     * or `null` for a dialog the bridge could not name, whose notification
     * names none.
     */
    val question: String?
        get() = when (subject) {
            is CarSubject.Dialog -> subject.pending.value
            CarSubject.UnnamedDialog -> null
            CarSubject.Turn -> NoticeNotification.TURN
        }

    /** The state of the notice it came from. */
    val state: NoticeState get() = if (subject == CarSubject.Turn) NoticeState.FINISHED else NoticeState.BLOCKED

    /** The intent data of this request. */
    fun toUri(): Uri =
        Uri.Builder().scheme(SCHEME).authority(HOST)
            .appendPath(action.wire)
            .appendPath(agent.value)
            .apply {
                when (subject) {
                    is CarSubject.Dialog -> appendPath(DIALOG).appendPath(subject.pending.value)
                    CarSubject.UnnamedDialog -> appendPath(BLOCKED)
                    CarSubject.Turn -> appendPath(TURN)
                }
            }
            .build()

    companion object {
        private const val SCHEME = "passporter"
        private const val HOST = "car"
        private const val DIALOG = "dialog"
        private const val BLOCKED = "blocked"
        private const val TURN = "turn"

        /** The request [uri] names, or `null` for anything else. */
        fun parse(uri: Uri?): CarRequest? {
            if (uri == null || uri.scheme != SCHEME || uri.host != HOST) return null
            val segments = uri.pathSegments
            val action = segments.getOrNull(0)?.let(CarAction::fromWire) ?: return null
            val agent = segments.getOrNull(1)?.let(AgentId::parse) ?: return null
            val subject = when (segments.drop(2).firstOrNull()) {
                DIALOG -> segments.getOrNull(3)?.let(PendingId::parse)?.let(CarSubject::Dialog)
                    ?.takeIf { segments.size == 4 }
                BLOCKED -> CarSubject.UnnamedDialog.takeIf { segments.size == 3 }
                TURN -> CarSubject.Turn.takeIf { segments.size == 3 }
                else -> null
            } ?: return null
            return CarRequest(action, agent, subject)
        }
    }
}
