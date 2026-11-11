package dev.passporter.notify.actions

import android.content.Context
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequest
import androidx.work.WorkManager
import java.util.concurrent.TimeUnit

/**
 * The native outbox (design §6.5): each answer from the shade is one
 * WorkManager job that waits for a network and retries with exponential
 * backoff ([OutboxWorker]). WorkManager keeps the job in its database, so
 * it survives the app being killed or the phone restarting.
 *
 * The unique work is one per question ([uniqueName]): a dialog takes one
 * answer, so a second tap while the first is queued is dropped
 * ([NoticeActionReceiver]) and two answers to one dialog never race;
 * replies to an agent are appended, so they go one after the other. A job
 * always ends in success, whatever its outcome, so it never fails the one
 * after it.
 */
object Outbox {
    /** The first retry's delay; each next one doubles. */
    const val BACKOFF_SECS = 30L

    /**
     * Attempts before giving up: about an hour of an unreachable bridge,
     * online (approximate: a followed `Retry-After` starts the backoff again).
     */
    const val MAX_ATTEMPTS = 8

    /** The longest `Retry-After` followed; a longer one is cut to it. */
    const val MAX_RETRY_AFTER_SECS = 15 * 60L

    /** Tags every outbox job, for tests and diagnostics. */
    const val TAG = "dev.passporter.notify.outbox"

    /** Beyond this WorkManager's five-hour cap applies anyway. */
    private const val MAX_SHIFT = 10

    /** Queues [job], and waits until WorkManager has it. Blocking. */
    fun enqueue(context: Context, job: OutboxJob) {
        val policy = if (job.request.action.answersDialog) ExistingWorkPolicy.KEEP else ExistingWorkPolicy.APPEND_OR_REPLACE
        WorkManager.getInstance(context)
            .enqueueUniqueWork(uniqueName(job.request), policy, request(job))
            .result.get()
    }

    /**
     * Queues [job]'s next attempt after [delaySecs] (cut to
     * [MAX_RETRY_AFTER_SECS]), to follow the running one: a bridge's
     * `Retry-After` longer than the backoff. Only for a dialog answer, whose
     * chain holds it alone. [attempts] is how many it has had so far. Not
     * waited on (a failure is shown when it comes): the worker that calls it
     * is still running, and its chain's update waits for that.
     */
    fun enqueueAfter(context: Context, job: OutboxJob, delaySecs: Long, attempts: Int) {
        val result = WorkManager.getInstance(context).enqueueUniqueWork(
            uniqueName(job.request),
            ExistingWorkPolicy.APPEND_OR_REPLACE,
            request(job, delaySecs.coerceAtMost(MAX_RETRY_AFTER_SECS), attempts),
        ).result
        val app = context.applicationContext
        result.addListener({
            try {
                result.get()
            } catch (e: Exception) {
                // WorkManager's database failed: the answer is lost; say so.
                ActionFeedback(app).notSent(job, NotSent.FAILED)
            }
        }, Runnable::run)
    }

    /** Whether an answer to [request]'s question is queued or running. Blocking. */
    fun isQueued(context: Context, request: ActionRequest): Boolean =
        WorkManager.getInstance(context).getWorkInfosForUniqueWork(uniqueName(request)).get()
            .any { !it.state.isFinished }

    /** Whether the job keyed [key] is queued or running. Blocking. */
    fun isQueued(context: Context, key: String): Boolean =
        WorkManager.getInstance(context).getWorkInfosByTag(keyTag(key)).get()
            .any { !it.state.isFinished }

    /** The unique work name of the answers to [request]'s question: its dialog, or the agent's turn. */
    fun uniqueName(request: ActionRequest): String =
        "notify-answer:${request.agent.value}:${request.pending?.value ?: "turn"}"

    /** The tag of the job keyed [key]. */
    fun keyTag(key: String): String = "notify-answer-key:$key"

    /** The work request of [job]: network-bound, backing off exponentially. */
    fun request(job: OutboxJob, delaySecs: Long = 0, attempts: Int = 0): OneTimeWorkRequest =
        OneTimeWorkRequest.Builder(OutboxWorker::class.java)
            .setInputData(OutboxData.write(job, attempts))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, BACKOFF_SECS, TimeUnit.SECONDS)
            .setInitialDelay(delaySecs, TimeUnit.SECONDS)
            .addTag(TAG)
            .addTag(keyTag(job.key))
            .build()

    /** The delay WorkManager gives the retry after attempt number [attempt] (0 for the first). */
    fun backoffSecs(attempt: Int): Long = BACKOFF_SECS shl attempt.coerceIn(0, MAX_SHIFT)
}
