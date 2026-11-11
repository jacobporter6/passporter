package dev.passporter.car

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.app.RemoteInput
import dev.passporter.notify.actions.ReplyText
import dev.passporter.notify.actions.Unsendable
import java.util.concurrent.Executor
import java.util.concurrent.Executors

/**
 * Receives Android Auto's two actions ([CarActions]). Not exported, so
 * only holders of this app's pending intents reach it: Android Auto, and
 * anything else that reads notifications (a watch, a notification
 * listener the person allowed, the Assistant). So it acts **only while
 * Android Auto projects** ([CarProjection]), and then also with the phone
 * locked: that is what the car needs. Off the car it sends nothing, and
 * a reply only says, quietly in the notification, to answer on the phone
 * ([CarSpeech.offTheCar]): the shade's own actions
 * ([dev.passporter.notify.actions.NoticeActions]) are the way there,
 * phone unlocked.
 *
 * - [CarAction.REPLY]: the utterance, made fit for the bridge's
 *   `Utterance` (one line, no hidden characters, at most
 *   [MAX_UTTERANCE_BYTES]) but otherwise as heard, becomes a [CarJob] with
 *   a fresh idempotency key, queued in [CarOutbox]. The bridge decides
 *   what it means under the car policy (design §6.6); the app adds
 *   nothing.
 * - [CarAction.MARK_AS_READ]: taken, and nothing else. Android Auto
 *   needs the action, the bridge has no API for it (a turn is unread
 *   until herdr sees the pane), and Android Auto fires it on a tap in the
 *   car, while it may still read the conversation or offer a reply, so
 *   the notification stays; the reconciler clears it as old news, as for
 *   any other.
 *
 * Handled one at a time, off the main thread.
 */
class CarReplyReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val request = CarRequest.parse(intent.data)
        if (intent.action != CarActions.INTENT_ACTION || request == null) {
            Log.w(TAG, "ignored an intent that is not a car action")
            return
        }
        val name = intent.getStringExtra(CarActions.EXTRA_NAME).orEmpty()
        val heard = if (request.action == CarAction.REPLY) heard(intent) else null
        val app = context.applicationContext
        // Null only when called directly (tests), not as a broadcast.
        val pending: PendingResult? = goAsync()
        executor.execute {
            try {
                take(app, request, name, heard)
            } finally {
                pending?.finish()
            }
        }
    }

    private fun take(context: Context, request: CarRequest, name: String, heard: CharSequence?) {
        if (!carProjection(context).isProjecting()) {
            Log.i(TAG, "ignored a car action while Android Auto is not projecting")
            if (request.action == CarAction.REPLY) CarSpeech(context).offTheCar(request, name)
            return
        }
        when (request.action) {
            CarAction.REPLY -> reply(context, request, name, heard)
            CarAction.MARK_AS_READ -> Log.i(TAG, "a conversation was read in the car")
        }
    }

    private fun reply(context: Context, request: CarRequest, name: String, heard: CharSequence?) {
        val utterance = when (val cleaned = ReplyText.clean(heard, singleLine = true, maxBytes = MAX_UTTERANCE_BYTES)) {
            is ReplyText.Result.Clean -> cleaned.text
            is ReplyText.Result.Problem -> {
                val trouble = if (cleaned.problem == Unsendable.TOO_LONG) CarTrouble.TOO_LONG else CarTrouble.NOT_HEARD
                return CarSpeech(context).trouble(request, name, null, trouble)
            }
        }
        val job = CarJob.create(request, utterance, name, System.currentTimeMillis())
        try {
            CarOutbox.enqueue(context, job)
        } catch (e: Exception) {
            // WorkManager's database failed: say so rather than stay silent.
            Log.e(TAG, "could not queue a car reply", e)
            CarSpeech(context).trouble(request, name, utterance, CarTrouble.FAILED)
        }
    }

    companion object {
        /** Most UTF-8 bytes of an utterance: the bridge's `MAX_UTTERANCE_BYTES`. */
        const val MAX_UTTERANCE_BYTES = 1024

        private const val TAG = "PassporterCar"

        /** Runs the actions, one at a time; tests run them in place. */
        internal var executor: Executor = Executors.newSingleThreadExecutor()

        /** The utterance Android Auto put into a reply, if any. */
        fun heard(intent: Intent): CharSequence? =
            RemoteInput.getResultsFromIntent(intent)?.getCharSequence(CarActions.KEY_UTTERANCE)
    }
}
