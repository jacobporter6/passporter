package dev.passporter.car

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import dev.passporter.app.R
import dev.passporter.notify.AgentId
import dev.passporter.notify.Notice

/**
 * The two actions Android Auto needs to take a messaging notification
 * (design §6.6, Google's "Extend messaging notifications to Android
 * Auto"), added as **invisible** actions, so the shade never shows them:
 *
 * | Action | Semantic action | `RemoteInput` key | Pending intent |
 * |---|---|---|---|
 * | [CarAction.REPLY] | `SEMANTIC_ACTION_REPLY` | [KEY_UTTERANCE] | mutable, explicit |
 * | [CarAction.MARK_AS_READ] | `SEMANTIC_ACTION_MARK_AS_READ` | — | immutable |
 *
 * Neither shows a user interface. They are the notification's only
 * semantic actions (the shade's own ones carry none), so the car can
 * never reach *Allow once*, *Deny* or anything else: everything it says
 * goes to the bridge as said, which maps it under the car policy.
 *
 * Only an announced notice ([Notice.announce]: the bridge applies
 * `car_announce_done`) gets them. Android Auto takes a messaging
 * notification only with a reply and a mark-as-read action, so a notice
 * without them is not read aloud in the car.
 *
 * Both go to [CarReplyReceiver], explicit and not exported, which acts
 * only while Android Auto projects ([CarProjection]). They do not ask to
 * unlock (Android Auto runs them with the phone locked in the car): the
 * projection check and the car-scope session stand in for it.
 */
object CarActions {
    /** The intent action of both. */
    const val INTENT_ACTION = "dev.passporter.car.ANSWER"

    /** The `RemoteInput` result key of [CarAction.REPLY]: the transcribed utterance. */
    const val KEY_UTTERANCE = "dev.passporter.car.UTTERANCE"

    /** The agent's name, for the conversation. Display only. */
    const val EXTRA_NAME = "dev.passporter.car.NAME"

    /**
     * The car actions of [notice], posted at [stamp]: reply and
     * mark-as-read if it is announced, else none.
     */
    fun build(context: Context, notice: Notice, stamp: Long): List<NotificationCompat.Action> {
        val subject = CarSubject.of(notice)?.takeIf { notice.announce } ?: return emptyList()
        return of(context, notice.agentId, subject, stamp, notice.name)
    }

    /** Reply and mark-as-read about [subject] of [agent], called [name], in the notice posted at [stamp]. */
    fun of(context: Context, agent: AgentId, subject: CarSubject, stamp: Long, name: String): List<NotificationCompat.Action> =
        CarAction.entries.map { action(context, CarRequest(it, agent, subject, stamp), name) }

    /** The explicit broadcast [request] sends. */
    fun intent(context: Context, request: CarRequest, name: String): Intent =
        Intent(context, CarReplyReceiver::class.java)
            .setAction(INTENT_ACTION)
            .setData(request.toUri())
            .putExtra(EXTRA_NAME, name)

    private fun action(context: Context, request: CarRequest, name: String): NotificationCompat.Action {
        val reply = request.action == CarAction.REPLY
        val mutability = if (reply) PendingIntent.FLAG_MUTABLE else PendingIntent.FLAG_IMMUTABLE
        val pendingIntent = PendingIntent.getBroadcast(
            context,
            0,
            intent(context, request, name),
            mutability or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val title = if (reply) R.string.car_action_reply else R.string.car_action_mark_as_read
        val builder = NotificationCompat.Action.Builder(null, context.getString(title), pendingIntent)
            .setSemanticAction(
                if (reply) {
                    NotificationCompat.Action.SEMANTIC_ACTION_REPLY
                } else {
                    NotificationCompat.Action.SEMANTIC_ACTION_MARK_AS_READ
                },
            )
            .setShowsUserInterface(false)
        if (reply) {
            builder.addRemoteInput(RemoteInput.Builder(KEY_UTTERANCE).setLabel(context.getString(R.string.car_reply_hint)).build())
                // Suggested replies would come from agent text: none.
                .setAllowGeneratedReplies(false)
        }
        return builder.build()
    }
}
