package dev.passporter.notify.actions

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import dev.passporter.app.R
import dev.passporter.notify.Notice

/**
 * The shade's notification actions of a [Notice] ([NoticeAction.of]):
 *
 * | Action | Title | Semantic action | `RemoteInput` key |
 * |---|---|---|---|
 * | [NoticeAction.REPLY] | Reply | none | [KEY_TEXT] |
 * | [NoticeAction.ALLOW_ONCE] | Allow once | none | — |
 * | [NoticeAction.DENY] | Deny | none | — |
 * | [NoticeAction.DENY_WITH_NOTE] | Deny with note | none | [KEY_TEXT] |
 *
 * None carries a semantic action, on purpose: a system feature that acts
 * on semantics (Android Auto, an assistant reading "reply" or "thumbs
 * up") must never allow or deny by guessing, nor answer through the shade.
 * The only semantic actions of a notification are Android Auto's reply
 * and mark-as-read ([dev.passporter.car.CarActions]), invisible, which
 * send the car's words to the bridge under the car policy. None shows a
 * user interface. Each needs the phone unlocked: Android 12 and later ask
 * before running it, and [NoticeActionReceiver] refuses while the phone is
 * locked on any version, also when a watch or another app fires it.
 *
 * Every action is a broadcast to [NoticeActionReceiver], explicit and not
 * exported, with the [ActionRequest] as its data. The pending intents are
 * immutable, except those with a `RemoteInput`, which Android must fill in
 * (mutable, so explicit).
 */
object NoticeActions {
    /** The intent action of every notification action. */
    const val INTENT_ACTION = "dev.passporter.notify.ANSWER"

    /** The `RemoteInput` result key of [NoticeAction.REPLY] and [NoticeAction.DENY_WITH_NOTE]. */
    const val KEY_TEXT = "dev.passporter.notify.TEXT"

    /** The agent's name, for the feedback in the notification. Display only. */
    const val EXTRA_NAME = "dev.passporter.notify.NAME"

    /** The actions of [notice], in order. */
    fun build(context: Context, notice: Notice): List<NotificationCompat.Action> =
        NoticeAction.of(notice).mapNotNull { action ->
            ActionRequest.of(action, notice)?.let { action(context, it, notice.name) }
        }

    /** The explicit broadcast [request] sends. */
    fun intent(context: Context, request: ActionRequest, name: String): Intent =
        Intent(context, NoticeActionReceiver::class.java)
            .setAction(INTENT_ACTION)
            .setData(request.toUri())
            .putExtra(EXTRA_NAME, name)

    private fun action(context: Context, request: ActionRequest, name: String): NotificationCompat.Action {
        val action = request.action
        val mutability = if (action.takesText) PendingIntent.FLAG_MUTABLE else PendingIntent.FLAG_IMMUTABLE
        val pendingIntent = PendingIntent.getBroadcast(
            context,
            0,
            intent(context, request, name),
            mutability or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val builder = NotificationCompat.Action.Builder(null, context.getString(title(action)), pendingIntent)
            .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_NONE)
            .setShowsUserInterface(false)
            .setAuthenticationRequired(true)
        if (action.takesText) {
            val hint = if (action == NoticeAction.REPLY) R.string.notify_reply_hint else R.string.notify_deny_note_hint
            builder.addRemoteInput(
                RemoteInput.Builder(KEY_TEXT).setLabel(context.getString(hint)).build(),
            )
                // Suggested replies would come from agent text: none.
                .setAllowGeneratedReplies(false)
        }
        return builder.build()
    }

    private fun title(action: NoticeAction): Int =
        when (action) {
            NoticeAction.REPLY -> R.string.notify_action_reply
            NoticeAction.ALLOW_ONCE -> R.string.notify_action_allow_once
            NoticeAction.DENY -> R.string.notify_action_deny
            NoticeAction.DENY_WITH_NOTE -> R.string.notify_action_deny_note
        }
}
