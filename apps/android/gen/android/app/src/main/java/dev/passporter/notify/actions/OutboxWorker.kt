package dev.passporter.notify.actions

import android.content.Context
import android.util.Log
import androidx.work.Worker
import androidx.work.WorkerParameters

/**
 * Runs one attempt of an [OutboxJob] ([OutboxSender]) and shows how it
 * went ([ActionFeedback]). A retry follows WorkManager's exponential
 * backoff, or the bridge's `Retry-After` when that is longer; after
 * [Outbox.MAX_ATTEMPTS] the job gives up as [NotSent.UNREACHABLE].
 */
class OutboxWorker internal constructor(
    context: Context,
    params: WorkerParameters,
    private val bridge: Bridge,
    private val frozen: FrozenBodies,
) : Worker(context, params) {
    /** The constructor WorkManager calls. */
    constructor(context: Context, params: WorkerParameters) :
        this(context, params, DeviceBridge(context.applicationContext), PreferenceFrozenBodies(context))

    override fun doWork(): Result {
        val job = OutboxData.read(inputData) ?: run {
            Log.w(TAG, "dropped an unreadable outbox job")
            return Result.failure()
        }
        val attempt = OutboxData.attempts(inputData) + runAttemptCount
        val feedback = ActionFeedback(applicationContext)
        return when (val outcome = OutboxSender(bridge, frozen).send(job)) {
            SendOutcome.Sent -> {
                feedback.sent(job)
                Reconciler.schedule(applicationContext)
                Result.success()
            }
            is SendOutcome.Final -> {
                Log.i(TAG, "answer not sent: ${outcome.reason}")
                feedback.notSent(job, outcome.reason)
                Result.failure()
            }
            is SendOutcome.Retry -> retry(job, attempt, outcome.afterSecs, feedback)
        }
    }

    private fun retry(job: OutboxJob, attempt: Int, afterSecs: Long?, feedback: ActionFeedback): Result {
        if (attempt + 1 >= Outbox.MAX_ATTEMPTS) {
            frozen.remove(job.key)
            feedback.notSent(job, NotSent.UNREACHABLE)
            return Result.failure()
        }
        if (afterSecs != null && afterSecs > Outbox.backoffSecs(runAttemptCount)) {
            // This run ends; the next one, appended to it, waits as asked.
            Outbox.enqueueAfter(applicationContext, job, afterSecs, attempt + 1)
            return Result.success()
        }
        return Result.retry()
    }

    private companion object {
        const val TAG = "PassporterNotify"
    }
}
