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
 * news replaced the notification meanwhile (another dialog, a new turn),
 * it stays as it is; it was announced itself. The look and the post hold
 * [NotificationLock].
 */
class CarSpeech(
    context: Context,
    private val now: () -> Long = System::currentTimeMillis,
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

    private fun post(request: CarRequest, name: String, utterance: String?, speech: String, keepShadeActions: Boolean) =
        synchronized(NotificationLock) {
            val base = active(request)
            if (base != null && base.extras.getString(NoticeNotification.EXTRA_QUESTION) != request.question) {
                Log.i(TAG, "newer news replaced the conversation; the car's answer is not shown")
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
            builder.addExtras(
                Bundle().apply {
                    request.question?.let { putString(NoticeNotification.EXTRA_QUESTION, it) }
                    // An answer from the shade still on its way keeps its mark.
                    base?.extras?.getString(ActionFeedback.EXTRA_JOB)?.let { putString(ActionFeedback.EXTRA_JOB, it) }
                },
            )
            if (keepShadeActions && base != null) {
                for (index in 0 until NotificationCompat.getActionCount(base)) {
                    NotificationCompat.getAction(base, index)?.let(builder::addAction)
                }
            }
            CarActions.of(context, request.agent, request.subject, name).forEach(builder::addInvisibleAction)
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
                CarTrouble.UNCERTAIN -> R.string.car_trouble_uncertain
                CarTrouble.FAILED -> R.string.car_trouble_failed
            }
    }
}
