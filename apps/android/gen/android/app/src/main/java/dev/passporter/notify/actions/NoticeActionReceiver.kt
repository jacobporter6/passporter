package dev.passporter.notify.actions

import android.app.KeyguardManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.app.RemoteInput
import dev.passporter.device.DeviceId
import java.util.concurrent.Executor
import java.util.concurrent.Executors

/**
 * Receives the notification actions ([NoticeActions]): turns one into an
 * [OutboxJob] with a fresh idempotency key, shows it as sending, and
 * queues it ([Outbox]). An action made for another pairing than the app's
 * now ([DeviceStamp]) queues nothing ([Unsendable.NOT_PAIRED]). Not exported, so only holders of this app's
 * pending intents reach it: the shade, and also a watch, a car or a
 * notification listener the person allowed. So it answers only while the
 * phone is unlocked, keyguard dismissed ([Unsendable.LOCKED]), whoever
 * fired the action.
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
        val device = DeviceStamp.of(intent.data)
        val name = intent.getStringExtra(NoticeActions.EXTRA_NAME).orEmpty()
        val typed = if (request.action.takesText) typed(intent) else null
        // isDeviceLocked is false while a trust agent (Smart Lock: a trusted
        // watch or car, on-body detection) keeps the phone unlockable without
        // a credential; the keyguard still shows then.
        val keyguard = context.getSystemService(KeyguardManager::class.java)
        val locked = keyguard.isDeviceLocked || keyguard.isKeyguardLocked
        val app = context.applicationContext
        // Null only when called directly (tests), not as a broadcast.
        val pending: PendingResult? = goAsync()
        executor.execute {
            try {
                take(app, request, device, name, typed, locked)
            } finally {
                pending?.finish()
            }
        }
    }

    private fun take(context: Context, request: ActionRequest, device: DeviceId?, name: String, typed: CharSequence?, locked: Boolean) {
        val feedback = ActionFeedback(context)
        if (locked) return feedback.refused(request, name, Unsendable.LOCKED)
        // Unsure what it is paired as (a busy Keystore): the sender checks again.
        if (device == null || pairedDevice(context).isNot(device)) return feedback.refused(request, name, Unsendable.NOT_PAIRED)
        val text = if (request.action.takesText) {
            when (val cleaned = ReplyText.clean(typed, request.action)) {
                is ReplyText.Result.Clean -> cleaned.text
                is ReplyText.Result.Problem -> return feedback.refused(request, name, cleaned.problem)
            }
        } else {
            null
        }
        if (request.action.answersDialog && queued(context, request)) {
            return feedback.refused(request, name, Unsendable.ALREADY_SENDING)
        }
        val job = OutboxJob.create(request, text, name, System.currentTimeMillis(), device)
        // Before the notification says sending: if the process dies before
        // the job is queued, this check finds it lost and says so.
        Reconciler.checkLeftoversLater(context)
        if (!feedback.sending(job)) {
            Log.i(TAG, "ignored a tap on a notice that is gone or that newer news replaced")
            return
        }
        try {
            Outbox.enqueue(context, job)
        } catch (e: Exception) {
            // WorkManager's database failed: say so rather than spin.
            Log.e(TAG, "could not queue an answer", e)
            feedback.notSent(job, NotSent.FAILED)
        }
    }

    /** [Outbox.isQueued]; a broken WorkManager counts as nothing queued, and enqueueing then says so. */
    private fun queued(context: Context, request: ActionRequest): Boolean =
        try {
            Outbox.isQueued(context, request)
        } catch (e: Exception) {
            Log.e(TAG, "could not read the outbox", e)
            false
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
