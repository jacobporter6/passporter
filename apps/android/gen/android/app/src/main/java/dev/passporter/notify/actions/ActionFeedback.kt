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
import dev.passporter.notify.AgentId
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
 *   none of the shade's actions, so it cannot be answered twice from
 *   there (Android Auto's stay: the car's replies go their own way);
 * - [sent]: the notification is removed: nothing waits for the person;
 * - [notSent]: a short reason (*Already answered*, *The agent moved on:
 *   open the app*, ...) under the conversation; a tap opens the app.
 *   Android Auto's actions stay, so the car can still take it;
 * - [lost]: a job that vanished before it was queued, shown as not sent.
 *
 * While its job runs, the notification carries the job's key
 * ([EXTRA_JOB]); a final outcome is marked [EXTRA_OUTCOME] instead, and
 * stays until the person dismisses or taps it, or news replaces it. Once
 * the car has spoken in the conversation meanwhile
 * ([dev.passporter.car.CarSpeech]), the key moves to [EXTRA_SPOKEN_OVER]:
 * the job's [sent] then leaves what the driver must hear (only the mark
 * goes), and its [notSent] adds its reason under it, so the person still
 * learns to answer again. If news replaced the notification meanwhile (a
 * new dialog, a new turn), the news stays as it is: whatever happened to the older answer, the news is what
 * matters now. A job's notification left over (its job lost) is cleared by
 * [Reconciler]. Every look-then-change holds [NotificationLock], as
 * [dev.passporter.notify.NoticeNotifier] does, so news posted meanwhile is
 * never overwritten.
 *
 * A tap with no notification behind it any more (cleared by a pairing
 * reset, say, between the tap's pairing check and [sending]) shows and
 * queues nothing; nor does an outcome come back for a job of another
 * pairing than the app's now ([paired], read under the lock): the
 * conversation belonged to the old bridge, and its tag may be the new
 * bridge's agent of the same id.
 */
class ActionFeedback(
    context: Context,
    private val now: () -> Long = System::currentTimeMillis,
    /** What the app is paired as now ([pairedDevice]); blocking. */
    private val paired: () -> PairedAs = { pairedDevice(context) },
) {
    private val context = context.applicationContext
    private val manager = NotificationManagerCompat.from(this.context)
    private val system = this.context.getSystemService(NotificationManager::class.java)

    /**
     * [job] is about to be queued: shown as sending, unless its notice is
     * gone (cleared by a pairing reset, say) or was replaced by news
     * meanwhile (another dialog, a new turn). Then nothing changes, and the
     * job must not be queued: `false`.
     */
    fun sending(job: OutboxJob): Boolean = synchronized(NotificationLock) {
        val base = active(job.request.agent)
        if (base == null || !asks(base, job.request)) return false
        repost(Conversation.of(job.request, job.name, base), answer(job.request.action, job.text), R.string.notify_sending, Mark.Job(job.key))
        true
    }

    /**
     * [job] reached the agent: its notification goes, unless the car spoke
     * in it since; then the car's words stay, no longer marked as the job's.
     */
    fun sent(job: OutboxJob) = synchronized(NotificationLock) {
        val base = active(job.request.agent) ?: return@synchronized
        when (job.key) {
            base.extras.getString(EXTRA_JOB) -> manager.cancel(NoticeNotification.tag(job.request.agent), NoticeNotification.ID)
            base.extras.getString(EXTRA_SPOKEN_OVER) -> repost(Conversation.of(job.request, job.name, base), null, null, null)
            else -> Unit
        }
    }

    /**
     * [job] was not sent, for [reason]. For another pairing than the app's
     * now, nothing comes back, and the job's own conversation, if it still
     * shows, goes (the pairing reset missed it).
     */
    fun notSent(job: OutboxJob, reason: NotSent) = synchronized(NotificationLock) {
        val base = active(job.request.agent)
        if (reason == NotSent.PAIRING_CHANGED || paired().isNot(job.device)) {
            if (base?.extras?.getString(EXTRA_JOB) == job.key) {
                manager.cancel(NoticeNotification.tag(job.request.agent), NoticeNotification.ID)
            }
            return@synchronized
        }
        when {
            // Gone while sending (swiped, or tapped into the app): only an
            // outcome that needs the person comes back.
            base == null -> if (reason in WORTH_RETURNING) {
                repost(Conversation.of(job.request, job.name, null), answer(job.request.action, job.text), message(reason), Mark.Outcome)
            }
            // The car spoke since: its words stay, the reason goes under them.
            base.extras.getString(EXTRA_JOB) == job.key || base.extras.getString(EXTRA_SPOKEN_OVER) == job.key ->
                repost(Conversation.of(job.request, job.name, base), null, message(reason), Mark.Outcome)
            else -> Log.i(TAG, "not sent ($reason); newer news replaced its notification")
        }
    }

    /**
     * The job keyed [key] is neither queued nor settled, yet [agent]'s
     * notification still shows it sending: its tap's process died between
     * showing it and queueing it. Shown as not sent ([NotSent.FAILED]), as
     * the person must answer again; anything else on show stays.
     */
    fun lost(agent: AgentId, key: String) = synchronized(NotificationLock) {
        val base = active(agent)?.takeIf { jobOf(it) == key } ?: return
        val name = base.extras.getCharSequence(Notification.EXTRA_TITLE)?.toString()
            ?: base.extras.getString(NoticeNotification.EXTRA_AGENT_NAME).orEmpty()
        // The base shows, so its own state and question stand; these are never used.
        val conversation = Conversation(agent, name, base, NoticeState.FINISHED, NoticeNotification.TURN)
        repost(conversation, null, message(NotSent.FAILED), Mark.Outcome)
    }

    /**
     * A tap that queued nothing ([Unsendable]): the notification stays as
     * it was, actions included (Android Auto's too), with the reason under
     * it (nothing for an empty entry), so a reply's spinner stops.
     */
    fun refused(request: ActionRequest, name: String, reason: Unsendable) = synchronized(NotificationLock) {
        val base = active(request.agent) ?: return
        val mark = base.extras.getString(EXTRA_JOB)?.let(Mark::Job)
            ?: base.extras.getString(EXTRA_SPOKEN_OVER)?.let(Mark::SpokenOver)
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
                Unsendable.NOT_PAIRED -> R.string.notify_not_sent_not_paired
            }
        }
        repost(Conversation.of(request, name, base), null, status, mark, keepShadeActions = true)
    }

    /** What a reposted notification says about the outbox. */
    private sealed interface Mark {
        /** It shows the progress of the job keyed [key]. */
        data class Job(val key: String) : Mark

        /** The car spoke over the progress of the job keyed [key] ([EXTRA_SPOKEN_OVER]). */
        data class SpokenOver(val key: String) : Mark

        /** It shows a job's final outcome. */
        data object Outcome : Mark
    }

    /**
     * The conversation a repost shows: [agent]'s, titled [name], on top of
     * [base] if it still shows, else a new one in [state] asking [question].
     */
    private class Conversation(
        val agent: AgentId,
        val name: String,
        val base: Notification?,
        val state: NoticeState,
        val question: String,
    ) {
        companion object {
            fun of(request: ActionRequest, name: String, base: Notification?) = Conversation(
                request.agent,
                name,
                base,
                if (request.action.answersDialog) NoticeState.BLOCKED else NoticeState.FINISHED,
                request.question,
            )
        }
    }

    /** Whether [notification] (news or an answer's progress) asks [request]'s question. */
    private fun asks(notification: Notification, request: ActionRequest): Boolean =
        notification.extras.getString(NoticeNotification.EXTRA_QUESTION) == request.question

    /**
     * The key of the job still on its way that [notification] shows,
     * spoken over by the car or not.
     */
    private fun jobOf(notification: Notification): String? =
        notification.extras.getString(EXTRA_JOB) ?: notification.extras.getString(EXTRA_SPOKEN_OVER)

    private fun active(agent: AgentId): Notification? {
        val tag = NoticeNotification.tag(agent)
        return system.activeNotifications
            .firstOrNull { it.tag == tag && it.id == NoticeNotification.ID }
            ?.notification
    }

    /**
     * Posts [conversation] again: its base's messages (or a new
     * conversation), plus [answer] as the person's message, with [status]
     * under it, quietly. Android Auto's actions stay (the car may still
     * talk, whatever the shade's answer came to); the shade's own only if
     * [keepShadeActions].
     */
    private fun repost(
        conversation: Conversation,
        answer: CharSequence?,
        status: Int?,
        mark: Mark?,
        keepShadeActions: Boolean = false,
    ) {
        val base = conversation.base
        val style = base?.let(NotificationCompat.MessagingStyle::extractMessagingStyleFromNotification)
            ?: NotificationCompat.MessagingStyle(
                Person.Builder().setName(context.getString(R.string.notify_you)).build(),
            )
        val time = now()
        answer?.let { style.addMessage(it, time, null as Person?) }
        val state = NoticeNotification.stateOf(base?.channelId) ?: conversation.state
        val builder = NoticeNotification.conversation(context, conversation.agent, state, style, time)
            .setContentTitle(conversation.name)
            .setOnlyAlertOnce(true)
            .setSilent(true)
        status?.let { builder.setSubText(context.getString(it)).setContentText(context.getString(it)) }
        val question = base?.extras?.getString(NoticeNotification.EXTRA_QUESTION) ?: conversation.question
        builder.addExtras(
            Bundle().apply {
                putString(NoticeNotification.EXTRA_QUESTION, question)
                // Still the same notice: Android Auto's actions name it.
                base?.extras?.takeIf { it.containsKey(NoticeNotification.EXTRA_STAMP) }
                    ?.let { putLong(NoticeNotification.EXTRA_STAMP, it.getLong(NoticeNotification.EXTRA_STAMP)) }
                base?.extras?.getString(NoticeNotification.EXTRA_AGENT_NAME)
                    ?.let { putString(NoticeNotification.EXTRA_AGENT_NAME, it) }
            },
        )
        when (mark) {
            is Mark.Job -> builder.addExtras(Bundle().apply { putString(EXTRA_JOB, mark.key) })
            is Mark.SpokenOver -> builder.addExtras(Bundle().apply { putString(EXTRA_SPOKEN_OVER, mark.key) })
            Mark.Outcome -> builder.addExtras(Bundle().apply { putBoolean(EXTRA_OUTCOME, true) })
            null -> Unit
        }
        if (keepShadeActions && base != null) {
            for (index in 0 until NotificationCompat.getActionCount(base)) {
                NotificationCompat.getAction(base, index)?.let(builder::addAction)
            }
        }
        if (base != null) {
            // Android Auto's, so the car can still take the conversation.
            NotificationCompat.getInvisibleActions(base).forEach(builder::addInvisibleAction)
        }
        try {
            manager.notify(NoticeNotification.tag(conversation.agent), NoticeNotification.ID, builder.build())
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

        /**
         * The extra that names the job whose progress a notification showed
         * before the car spoke in it ([dev.passporter.car.CarSpeech]): its
         * outcome may add a reason, never clear or replace the car's words.
         */
        const val EXTRA_SPOKEN_OVER = "dev.passporter.notify.SPOKEN_OVER"

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
                NotSent.NOT_PAIRED, NotSent.PAIRING_CHANGED -> R.string.notify_not_sent_not_paired
                NotSent.UNREACHABLE -> R.string.notify_not_sent_unreachable
                NotSent.FAILED -> R.string.notify_not_sent_failed
            }
    }
}
