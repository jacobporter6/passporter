package dev.passporter.notify.actions

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import dev.passporter.app.R
import dev.passporter.notify.Notice

/**
 * The notification actions of a [Notice] ([NoticeAction.of]), as Android
 * wants them for a messaging notification, so Android Auto (design §6.6)
 * can reuse them:
 *
 * | Action | Title | Semantic action | `RemoteInput` key |
 * |---|---|---|---|
 * | [NoticeAction.REPLY] | Reply | `SEMANTIC_ACTION_REPLY` | [KEY_TEXT] |
 * | [NoticeAction.ALLOW_ONCE] | Allow once | none | — |
 * | [NoticeAction.DENY] | Deny | none | — |
 * | [NoticeAction.DENY_WITH_NOTE] | Deny with note | none | [KEY_TEXT] |
 *
 * The dialog answers carry no semantic action on purpose: a system
 * feature that acts on semantics (a car or an assistant reading "reply"
 * or "thumbs up") must never allow or deny by guessing. None shows a user
 * interface, and each needs the phone unlocked: someone holding a locked
 * phone cannot answer an agent.
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
            .setSemanticAction(
                if (action == NoticeAction.REPLY) {
                    NotificationCompat.Action.SEMANTIC_ACTION_REPLY
                } else {
                    NotificationCompat.Action.SEMANTIC_ACTION_NONE
                },
            )
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
