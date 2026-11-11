package dev.passporter.notify.actions

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.app.RemoteInput

/**
 * Receives the notification actions ([NoticeActions]): turns one into an
 * [OutboxJob] with a fresh idempotency key, shows it as sending, and
 * queues it ([Outbox]). Not exported: only this app's pending intents
 * reach it.
 */
class NoticeActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val request = ActionRequest.parse(intent.data)
        if (intent.action != NoticeActions.INTENT_ACTION || request == null) {
            Log.w(TAG, "ignored an intent that is not a notification action")
            return
        }
        val name = intent.getStringExtra(NoticeActions.EXTRA_NAME).orEmpty()
        val feedback = ActionFeedback(context)
        val text = if (request.action.takesText) {
            when (val cleaned = ReplyText.clean(typed(intent), request.action)) {
                is ReplyText.Result.Clean -> cleaned.text
                is ReplyText.Result.Problem -> {
                    feedback.refused(request, name, cleaned.problem)
                    return
                }
            }
        } else {
            null
        }
        val job = OutboxJob.create(request, text, name, System.currentTimeMillis())
        feedback.sending(job)
        Outbox.enqueue(context, job)
    }

    companion object {
        private const val TAG = "PassporterNotify"

        /** The text typed or spoken into a notification action, if any. */
        fun typed(intent: Intent): CharSequence? =
            RemoteInput.getResultsFromIntent(intent)?.getCharSequence(NoticeActions.KEY_TEXT)
    }
}
