package dev.passporter.car

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
import dev.passporter.notify.NotificationLock
import dev.passporter.notify.actions.ActionFeedback

/**
 * Puts the answer to a spoken reply into the agent's conversation, so
 * Android Auto reads it aloud: the utterance as the person's message,
 * then one sentence from Passporter (the bridge's, "Allowed: cargo test",
 * or the app's own when the bridge gave none, [CarTrouble]). It alerts,
 * as a new message must for the car to announce it, and keeps the car
 * actions, so the person can answer again (after "Say just yes to allow",
 * say). The shade's own actions stay unless the reply was sent: then the
 * question is answered.
 *
 * News wins, as for answers from the shade ([ActionFeedback]): if newer
 * news replaced the notification meanwhile (another dialog, a new turn:
 * any other notice, by its [NoticeNotification.EXTRA_STAMP]), it stays as
 * it is, announced or not. If the notification is gone (a heard turn,
 * swiped, or cleared as old news), the answer comes back only while
 * Android Auto projects: the driver must hear it, and off the car old news
 * stays cleared. The look and the post hold [NotificationLock].
 */
class CarSpeech(
    context: Context,
    private val now: () -> Long = System::currentTimeMillis,
    private val projection: CarProjection = carProjection(context),
) {
    private val context = context.applicationContext
    private val manager = NotificationManagerCompat.from(this.context)
    private val system = this.context.getSystemService(NotificationManager::class.java)

    /** The bridge answered [job] with [speech]; [sent] when it reached the agent. */
    fun answer(job: CarJob, speech: String, sent: Boolean) =
        post(job.request, job.name, job.utterance, speech, keepShadeActions = !sent)

    /**
     * The app answers [request] itself, for [trouble]; [utterance] is what
     * was heard, if it was fit to show.
     */
    fun trouble(request: CarRequest, name: String, utterance: String?, trouble: CarTrouble) =
        post(request, name, utterance, context.getString(message(trouble)), keepShadeActions = true)

    /**
     * A car action fired while Android Auto does not project (a watch, the
     * Assistant): nothing was sent. The notification stays as it was,
     * actions included, quietly saying to answer on the phone, so whoever
     * fired it is not left believing it went.
     */
    fun offTheCar(request: CarRequest, name: String) = synchronized(NotificationLock) {
        val base = active(request)?.takeIf { shows(it, request) } ?: return@synchronized
        val style = NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(base) ?: return@synchronized
        val status = context.getString(R.string.car_answer_on_phone)
        val state = NoticeNotification.stateOf(base.channelId) ?: request.state
        val builder = NoticeNotification.conversation(context, request.agent, state, style, now())
            .setContentTitle(name)
            .setSubText(status)
            .setContentText(status)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .addExtras(base.extras.marks())
        copyActions(base, builder, visible = true)
        notify(request, builder)
    }

    private fun post(request: CarRequest, name: String, utterance: String?, speech: String, keepShadeActions: Boolean) =
        synchronized(NotificationLock) {
            val base = active(request)
            if (base != null && !shows(base, request)) {
                Log.i(TAG, "newer news replaced the conversation; the car's answer is not shown")
                return@synchronized
            }
            if (base == null && !projection.isProjecting()) {
                Log.i(TAG, "the conversation is gone and the car with it; the car's answer is not shown")
                return@synchronized
            }
            val style = base?.let(NotificationCompat.MessagingStyle::extractMessagingStyleFromNotification)
                ?: NotificationCompat.MessagingStyle(Person.Builder().setName(context.getString(R.string.notify_you)).build())
            // One agent, one conversation, whoever else speaks in it.
            style.setGroupConversation(false)
            val time = now()
            utterance?.let { style.addMessage(it, time, null as Person?) }
            style.addMessage(speech, time, passporter())
            val state = NoticeNotification.stateOf(base?.channelId) ?: request.state
            val builder = NoticeNotification.conversation(context, request.agent, state, style, time)
                .setContentTitle(name)
                .setContentText(speech)
                // A new message: the car announces it.
                .setOnlyAlertOnce(false)
            builder.addExtras(base?.extras?.marks() ?: Bundle().apply { stamp(request) })
            if (base != null) {
                copyActions(base, builder, visible = keepShadeActions)
            } else {
                CarActions.of(context, request.agent, request.subject, request.stamp, name)
                    .forEach(builder::addInvisibleAction)
            }
            notify(request, builder)
        }

    /** Whether [notification] still shows the notice [request] is about. */
    private fun shows(notification: Notification, request: CarRequest): Boolean {
        val extras = notification.extras
        return extras.getString(NoticeNotification.EXTRA_QUESTION) == request.question &&
            extras.containsKey(NoticeNotification.EXTRA_STAMP) &&
            extras.getLong(NoticeNotification.EXTRA_STAMP) == request.stamp
    }

    /**
     * What a repost keeps of this notice's extras: its question and stamp,
     * and the mark of an answer from the shade still on its way.
     */
    private fun Bundle.marks(): Bundle = Bundle().also { kept ->
        getString(NoticeNotification.EXTRA_QUESTION)?.let { kept.putString(NoticeNotification.EXTRA_QUESTION, it) }
        kept.putLong(NoticeNotification.EXTRA_STAMP, getLong(NoticeNotification.EXTRA_STAMP))
        getString(ActionFeedback.EXTRA_JOB)?.let { kept.putString(ActionFeedback.EXTRA_JOB, it) }
    }

    private fun Bundle.stamp(request: CarRequest) {
        request.question?.let { putString(NoticeNotification.EXTRA_QUESTION, it) }
        putLong(NoticeNotification.EXTRA_STAMP, request.stamp)
    }

    /**
     * Copies [base]'s car actions, which it has since it shows an announced
     * notice, and its shade actions if [visible].
     */
    private fun copyActions(base: Notification, builder: NotificationCompat.Builder, visible: Boolean) {
        if (visible) {
            for (index in 0 until NotificationCompat.getActionCount(base)) {
                NotificationCompat.getAction(base, index)?.let(builder::addAction)
            }
        }
        NotificationCompat.getInvisibleActions(base).forEach(builder::addInvisibleAction)
    }

    private fun notify(request: CarRequest, builder: NotificationCompat.Builder) {
        try {
            manager.notify(NoticeNotification.tag(request.agent), NoticeNotification.ID, builder.build())
        } catch (_: SecurityException) {
            // Notifications were turned off meanwhile: nothing to show.
        }
    }

    private fun active(request: CarRequest): Notification? {
        val tag = NoticeNotification.tag(request.agent)
        return system.activeNotifications
            .firstOrNull { it.tag == tag && it.id == NoticeNotification.ID }
            ?.notification
    }

    /** Passporter, the sender of the sentences read aloud: not the agent, not the person. */
    private fun passporter(): Person =
        Person.Builder()
            .setName(context.getString(R.string.app_name))
            .setKey(PASSPORTER_KEY)
            .setBot(true)
            .build()

    companion object {
        /** The [Person] key of Passporter's own messages. */
        const val PASSPORTER_KEY = "dev.passporter"

        private const val TAG = "PassporterCar"

        /** The sentence read aloud for [trouble]. */
        fun message(trouble: CarTrouble): Int =
            when (trouble) {
                CarTrouble.NOT_HEARD -> R.string.car_trouble_not_heard
                CarTrouble.TOO_LONG -> R.string.car_trouble_too_long
                CarTrouble.NOT_PAIRED -> R.string.car_trouble_not_paired
                CarTrouble.UNREACHABLE -> R.string.car_trouble_unreachable
                CarTrouble.TOO_OLD -> R.string.car_trouble_too_old
                CarTrouble.UNCERTAIN -> R.string.car_trouble_uncertain
                CarTrouble.FAILED -> R.string.car_trouble_failed
            }
    }
}
