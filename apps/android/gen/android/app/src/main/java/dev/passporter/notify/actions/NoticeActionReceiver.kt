package dev.passporter.notify.actions

import android.app.KeyguardManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.app.RemoteInput
import java.util.concurrent.Executor
import java.util.concurrent.Executors

/**
 * Receives the notification actions ([NoticeActions]): turns one into an
 * [OutboxJob] with a fresh idempotency key, shows it as sending, and
 * queues it ([Outbox]). Not exported, so only holders of this app's
 * pending intents reach it: the shade, and also a watch, a car or a
 * notification listener the person allowed. So it answers only while the
 * phone is unlocked ([Unsendable.LOCKED]), whoever fired the action.
 *
 * Taps are handled one at a time, off the main thread: a second tap on a
 * dialog's notification sees the first one's job and queues nothing.
 */
class NoticeActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val request = ActionRequest.parse(intent.data)
        if (intent.action != NoticeActions.INTENT_ACTION || request == null) {
            Log.w(TAG, "ignored an intent that is not a notification action")
            return
        }
        val name = intent.getStringExtra(NoticeActions.EXTRA_NAME).orEmpty()
        val typed = if (request.action.takesText) typed(intent) else null
        val locked = context.getSystemService(KeyguardManager::class.java).isDeviceLocked
        val app = context.applicationContext
        // Null only when called directly (tests), not as a broadcast.
        val pending: PendingResult? = goAsync()
        executor.execute {
            try {
                take(app, request, name, typed, locked)
            } finally {
                pending?.finish()
            }
        }
    }

    private fun take(context: Context, request: ActionRequest, name: String, typed: CharSequence?, locked: Boolean) {
        val feedback = ActionFeedback(context)
        if (locked) return feedback.refused(request, name, Unsendable.LOCKED)
        val text = if (request.action.takesText) {
            when (val cleaned = ReplyText.clean(typed, request.action)) {
                is ReplyText.Result.Clean -> cleaned.text
                is ReplyText.Result.Problem -> return feedback.refused(request, name, cleaned.problem)
            }
        } else {
            null
        }
        if (request.action.answersDialog && Outbox.isQueued(context, request)) {
            return feedback.refused(request, name, Unsendable.ALREADY_SENDING)
        }
        val job = OutboxJob.create(request, text, name, System.currentTimeMillis())
        feedback.sending(job)
        Outbox.enqueue(context, job)
    }

    companion object {
        private const val TAG = "PassporterNotify"

        /** Runs the taps, one at a time; tests run them in place. */
        internal var executor: Executor = Executors.newSingleThreadExecutor()

        /** The text typed or spoken into a notification action, if any. */
        fun typed(intent: Intent): CharSequence? =
            RemoteInput.getResultsFromIntent(intent)?.getCharSequence(NoticeActions.KEY_TEXT)
    }
}
