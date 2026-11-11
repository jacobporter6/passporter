package dev.passporter.notify.actions

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.os.Bundle
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import dev.passporter.app.R
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.NotificationLock

/**
 * Shows the progress of an answer given from the shade in the agent's own
 * notification, quietly (it never alerts):
 *
 * - [sending]: the answer is appended to the conversation as the person's
 *   message (Android expects that after a `RemoteInput` reply; until the
 *   notification is updated its send button spins), with *Sending…* and
 *   no actions, so it cannot be answered twice;
 * - [sent]: the notification is removed: nothing waits for the person;
 * - [notSent]: a short reason (*Already answered*, *The agent moved on:
 *   open the app*, ...) under the conversation; a tap opens the app.
 *
 * While its job runs, the notification carries the job's key
 * ([EXTRA_JOB]); a final outcome is marked [EXTRA_OUTCOME] instead, and
 * stays until the person dismisses or taps it, or news replaces it. If news
 * replaced the notification meanwhile (a new dialog, a new turn), the news
 * stays as it is: whatever happened to the older answer, the news is what
 * matters now. A job's notification left over (its job lost) is cleared by
 * [Reconciler]. Every look-then-change holds [NotificationLock], as
 * [dev.passporter.notify.NoticeNotifier] does, so news posted meanwhile is
 * never overwritten.
 */
class ActionFeedback(
    context: Context,
    private val now: () -> Long = System::currentTimeMillis,
) {
    private val context = context.applicationContext
    private val manager = NotificationManagerCompat.from(this.context)
    private val system = this.context.getSystemService(NotificationManager::class.java)

    /**
     * [job] is about to be queued: shown as sending, unless its notice was
     * replaced by news meanwhile (another dialog, a new turn). Then nothing
     * changes, and the job must not be queued: `false`.
     */
    fun sending(job: OutboxJob): Boolean = synchronized(NotificationLock) {
        val base = active(job.request)
        if (base != null && !asks(base, job.request)) return false
        repost(job.request, job.name, base, answer(job.request.action, job.text), R.string.notify_sending, Mark.Job(job.key))
        true
    }

    /** [job] reached the agent. */
    fun sent(job: OutboxJob) = synchronized(NotificationLock) {
        if (isOwnedBy(job)) manager.cancel(NoticeNotification.tag(job.request.agent), NoticeNotification.ID)
    }

    /** [job] was not sent, for [reason]. */
    fun notSent(job: OutboxJob, reason: NotSent) = synchronized(NotificationLock) {
        val base = active(job.request)
        when {
            // Gone while sending (swiped, or tapped into the app): only an
            // outcome that needs the person comes back.
            base == null -> if (reason in WORTH_RETURNING) {
                repost(job.request, job.name, null, answer(job.request.action, job.text), message(reason), Mark.Outcome)
            }
            base.extras.getString(EXTRA_JOB) == job.key -> repost(job.request, job.name, base, null, message(reason), Mark.Outcome)
            else -> Log.i(TAG, "not sent ($reason); newer news replaced its notification")
        }
    }

    /**
     * A tap that queued nothing ([Unsendable]): the notification stays as
     * it was, actions included (Android Auto's too), with the reason under
     * it (nothing for an empty entry), so a reply's spinner stops.
     */
    fun refused(request: ActionRequest, name: String, reason: Unsendable) = synchronized(NotificationLock) {
        val base = active(request) ?: return
        val mark = base.extras.getString(EXTRA_JOB)?.let(Mark::Job)
            ?: Mark.Outcome.takeIf { base.extras.getBoolean(EXTRA_OUTCOME) }
        // Newer news is reposted as it is, which only stops the spinner.
        val status = if (!asks(base, request)) {
            null
        } else {
            when (reason) {
                Unsendable.EMPTY -> null
                Unsendable.ALREADY_SENDING -> R.string.notify_sending
                Unsendable.TOO_LONG -> R.string.notify_not_sent_too_long
                Unsendable.LOCKED -> R.string.notify_unlock_to_answer
            }
        }
        repost(request, name, base, null, status, mark, keepActions = true)
    }

    /** What a reposted notification says about the outbox. */
    private sealed interface Mark {
        /** It shows the progress of the job keyed [key]. */
        data class Job(val key: String) : Mark

        /** It shows a job's final outcome. */
        data object Outcome : Mark
    }

    /** Whether [notification] (news or an answer's progress) asks [request]'s question. */
    private fun asks(notification: Notification, request: ActionRequest): Boolean =
        notification.extras.getString(NoticeNotification.EXTRA_QUESTION) == request.question

    private fun isOwnedBy(job: OutboxJob): Boolean = active(job.request)?.extras?.getString(EXTRA_JOB) == job.key

    private fun active(request: ActionRequest): Notification? {
        val tag = NoticeNotification.tag(request.agent)
        return system.activeNotifications
            .firstOrNull { it.tag == tag && it.id == NoticeNotification.ID }
            ?.notification
    }

    /**
     * Posts [request]'s conversation again: [base]'s messages (or a new
     * conversation), plus [answer] as the person's message, with [status]
     * under it, quietly.
     */
    private fun repost(
        request: ActionRequest,
        name: String,
        base: Notification?,
        answer: CharSequence?,
        status: Int?,
        mark: Mark?,
        keepActions: Boolean = false,
    ) {
        val style = base?.let(NotificationCompat.MessagingStyle::extractMessagingStyleFromNotification)
            ?: NotificationCompat.MessagingStyle(
                Person.Builder().setName(context.getString(R.string.notify_you)).build(),
            )
        val time = now()
        answer?.let { style.addMessage(it, time, null as Person?) }
        val state = NoticeNotification.stateOf(base?.channelId)
            ?: if (request.action.answersDialog) NoticeState.BLOCKED else NoticeState.FINISHED
        val builder = NoticeNotification.conversation(context, request.agent, state, style, time)
            .setContentTitle(name)
            .setOnlyAlertOnce(true)
            .setSilent(true)
        status?.let { builder.setSubText(context.getString(it)).setContentText(context.getString(it)) }
        val question = base?.extras?.getString(NoticeNotification.EXTRA_QUESTION) ?: request.question
        builder.addExtras(
            Bundle().apply {
                putString(NoticeNotification.EXTRA_QUESTION, question)
                // Still the same notice: Android Auto's actions name it.
                base?.extras?.takeIf { it.containsKey(NoticeNotification.EXTRA_STAMP) }
                    ?.let { putLong(NoticeNotification.EXTRA_STAMP, it.getLong(NoticeNotification.EXTRA_STAMP)) }
            },
        )
        when (mark) {
            is Mark.Job -> builder.addExtras(Bundle().apply { putString(EXTRA_JOB, mark.key) })
            Mark.Outcome -> builder.addExtras(Bundle().apply { putBoolean(EXTRA_OUTCOME, true) })
            null -> Unit
        }
        if (keepActions && base != null) {
            for (index in 0 until NotificationCompat.getActionCount(base)) {
                NotificationCompat.getAction(base, index)?.let(builder::addAction)
            }
            // Android Auto's, so the car can still take the conversation.
            NotificationCompat.getInvisibleActions(base).forEach(builder::addInvisibleAction)
        }
        try {
            manager.notify(NoticeNotification.tag(request.agent), NoticeNotification.ID, builder.build())
        } catch (_: SecurityException) {
            // Notifications were turned off meanwhile: nothing to show.
        }
    }

    /** The person's answer, as their message in the conversation. */
    private fun answer(action: NoticeAction, text: String?): CharSequence =
        when (action) {
            NoticeAction.REPLY -> requireNotNull(text)
            NoticeAction.ALLOW_ONCE -> context.getString(R.string.notify_action_allow_once)
            NoticeAction.DENY -> context.getString(R.string.notify_action_deny)
            NoticeAction.DENY_WITH_NOTE -> context.getString(R.string.notify_you_denied_with, requireNotNull(text))
        }

    companion object {
        /** The extra that names the job whose progress a notification shows. */
        const val EXTRA_JOB = "dev.passporter.notify.JOB"

        /** The extra that marks a notification showing an answer's final outcome. */
        const val EXTRA_OUTCOME = "dev.passporter.notify.OUTCOME"

        private const val TAG = "PassporterNotify"

        /** Outcomes shown even after the notification went: the person must act on them. */
        private val WORTH_RETURNING = setOf(NotSent.UNCERTAIN, NotSent.UNREACHABLE, NotSent.NOT_PAIRED, NotSent.FAILED, NotSent.NEEDS_APP)

        /** The short reason shown for [reason]. */
        fun message(reason: NotSent): Int =
            when (reason) {
                NotSent.ALREADY_ANSWERED -> R.string.notify_not_sent_answered
                NotSent.MOVED_ON -> R.string.notify_not_sent_moved_on
                NotSent.AGENT_GONE -> R.string.notify_not_sent_gone
                NotSent.NEEDS_APP -> R.string.notify_not_sent_needs_app
                NotSent.UNCERTAIN -> R.string.notify_not_sent_uncertain
                NotSent.NOT_PAIRED -> R.string.notify_not_sent_not_paired
                NotSent.UNREACHABLE -> R.string.notify_not_sent_unreachable
                NotSent.FAILED -> R.string.notify_not_sent_failed
            }
    }
}
