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
import dev.passporter.device.DeviceId
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
 * it is, announced or not. If the notification is gone (swiped, or
 * cleared as old news), the answer comes back while Android Auto projects
 * (the driver must hear it); off the car only one the person must act on
 * comes back (refused, not sent, unsure, not paired), and other old news
 * stays cleared; off the car an answer never alerts. An answer for an old
 * pairing ([CarTrouble.PAIRING_CHANGED]) never comes back. The title is the
 * notification's own while it shows; the name in the intent (display
 * only, and fillable by anything holding the reply intent) is used only to
 * bring a gone conversation back. The look and the post hold [NotificationLock].
 *
 * A repost is the car's turn in the conversation: it no longer shows an
 * answer from the shade on its way as such ([ActionFeedback.EXTRA_JOB]
 * becomes [ActionFeedback.EXTRA_SPOKEN_OVER], so that job's being sent
 * cannot clear what the driver must hear, and its not being sent only
 * adds its reason), and keeps the mark of an outcome already shown
 * ([ActionFeedback.EXTRA_OUTCOME]), which is not news.
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
        post(job.request, job.name, job.utterance, speech, job.device, keepShadeActions = !sent, mustReturn = !sent)

    /**
     * The app answers [request], made for [device], itself, for [trouble];
     * [utterance] is what was heard, if it was fit to show.
     */
    fun trouble(request: CarRequest, name: String, utterance: String?, trouble: CarTrouble, device: DeviceId?) =
        post(
            request,
            name,
            utterance,
            context.getString(message(trouble)),
            device.takeIf { trouble != CarTrouble.PAIRING_CHANGED },
            keepShadeActions = true,
            mustReturn = trouble in MUST_RETURN,
        )

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
            .setContentTitle(base.extras.getString(NoticeNotification.EXTRA_AGENT_NAME) ?: name)
            .setSubText(status)
            .setContentText(status)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            // The car said nothing: an answer from the shade on its way stays as it is.
            .addExtras(base.extras.marks(spoken = false))
        copyActions(base, builder, visible = true)
        notify(request, builder)
    }

    /**
     * Posts [speech] into [request]'s conversation. [device] is the pairing
     * the answer belongs to: a gone conversation is brought back only with
     * one, and with car actions made for it.
     */
    private fun post(
        request: CarRequest,
        name: String,
        utterance: String?,
        speech: String,
        device: DeviceId?,
        keepShadeActions: Boolean,
        mustReturn: Boolean,
    ) =
        synchronized(NotificationLock) {
            val base = active(request)
            if (base != null && !shows(base, request)) {
                Log.i(TAG, "newer news replaced the conversation; the car's answer is not shown")
                return@synchronized
            }
            if (base == null && device == null) {
                Log.i(TAG, "the conversation is gone, and belonged to another pairing; the car's answer is not shown")
                return@synchronized
            }
            val quiet = !projection.isProjecting()
            if (base == null && quiet && !mustReturn) {
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
                .setContentTitle(base?.extras?.getString(NoticeNotification.EXTRA_AGENT_NAME) ?: name)
                .setContentText(speech)
                // A new message: the car announces it. Off the car, back on the phone quietly.
                .setOnlyAlertOnce(quiet)
                .setSilent(quiet)
            builder.addExtras(base?.extras?.marks(spoken = true) ?: Bundle().apply { stamp(request, name) })
            if (base != null) {
                copyActions(base, builder, visible = keepShadeActions)
            } else if (device != null) {
                CarActions.of(context, request.agent, request.subject, request.stamp, name, device)
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
     * What a repost keeps of this notice's extras: its question, stamp and
     * agent name, the mark of an outcome shown (not news), and an answer
     * from the shade on its way: as spoken over if the car [spoken] in the
     * repost (see the class comment), else as it was.
     */
    private fun Bundle.marks(spoken: Boolean): Bundle = Bundle().also { kept ->
        getString(NoticeNotification.EXTRA_QUESTION)?.let { kept.putString(NoticeNotification.EXTRA_QUESTION, it) }
        kept.putLong(NoticeNotification.EXTRA_STAMP, getLong(NoticeNotification.EXTRA_STAMP))
        getString(NoticeNotification.EXTRA_AGENT_NAME)?.let { kept.putString(NoticeNotification.EXTRA_AGENT_NAME, it) }
        if (getBoolean(ActionFeedback.EXTRA_OUTCOME)) kept.putBoolean(ActionFeedback.EXTRA_OUTCOME, true)
        getString(ActionFeedback.EXTRA_SPOKEN_OVER)?.let { kept.putString(ActionFeedback.EXTRA_SPOKEN_OVER, it) }
        getString(ActionFeedback.EXTRA_JOB)?.let {
            kept.putString(if (spoken) ActionFeedback.EXTRA_SPOKEN_OVER else ActionFeedback.EXTRA_JOB, it)
        }
    }

    private fun Bundle.stamp(request: CarRequest, name: String) {
        request.question?.let { putString(NoticeNotification.EXTRA_QUESTION, it) }
        putLong(NoticeNotification.EXTRA_STAMP, request.stamp)
        putString(NoticeNotification.EXTRA_AGENT_NAME, name)
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

        /** Troubles shown even after the conversation went: the person must act on them. */
        private val MUST_RETURN = setOf(
            CarTrouble.NOT_PAIRED,
            CarTrouble.UNREACHABLE,
            CarTrouble.TOO_OLD,
            CarTrouble.UNCERTAIN,
            CarTrouble.FAILED,
        )

        /** The sentence read aloud for [trouble]. */
        fun message(trouble: CarTrouble): Int =
            when (trouble) {
                CarTrouble.NOT_HEARD -> R.string.car_trouble_not_heard
                CarTrouble.TOO_LONG -> R.string.car_trouble_too_long
                CarTrouble.NOT_PAIRED, CarTrouble.PAIRING_CHANGED -> R.string.car_trouble_not_paired
                CarTrouble.UNREACHABLE -> R.string.car_trouble_unreachable
                CarTrouble.TOO_OLD -> R.string.car_trouble_too_old
                CarTrouble.UNCERTAIN -> R.string.car_trouble_uncertain
                CarTrouble.FAILED -> R.string.car_trouble_failed
            }
    }
}
