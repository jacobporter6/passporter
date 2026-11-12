package dev.passporter.notify.actions

import android.content.Context
import android.util.Log
import androidx.work.Worker
import androidx.work.WorkerParameters

/**
 * Runs one attempt of an [OutboxJob] ([OutboxSender]) and shows how it
 * went ([ActionFeedback]). A retry follows WorkManager's exponential
 * backoff, or, for a dialog answer, the bridge's `Retry-After` when that
 * is longer (up to [Outbox.MAX_RETRY_AFTER_SECS]); after
 * [Outbox.MAX_ATTEMPTS] the job gives up as [NotSent.UNREACHABLE] (or
 * [NotSent.UNCERTAIN] once a body was sent).
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
        // Every outcome is a success for WorkManager: a failed link would
        // fail the replies appended after it unrun (Outbox).
        val job = OutboxData.read(inputData) ?: run {
            Log.w(TAG, "dropped an unreadable outbox job")
            return Result.success()
        }
        val attempt = OutboxData.attempts(inputData) + runAttemptCount
        val feedback = ActionFeedback(applicationContext)
        val outcome = OutboxSender(bridge, frozen) { pairedDevice(applicationContext) }.send(job, ::isStopped)
        // Stopped meanwhile: WorkManager ignores this run and runs the job
        // again, so nothing may show or settle (the frozen body stays).
        if (isStopped) return Result.retry()
        return when (outcome) {
            SendOutcome.Sent -> {
                feedback.sent(job)
                Reconciler.schedule(applicationContext)
                Result.success()
            }
            is SendOutcome.Final -> {
                Log.i(TAG, "answer not sent: ${outcome.reason}")
                feedback.notSent(job, outcome.reason)
                Result.success()
            }
            is SendOutcome.Retry -> retry(job, attempt, outcome.afterSecs, feedback)
        }
    }

    private fun retry(job: OutboxJob, attempt: Int, afterSecs: Long?, feedback: ActionFeedback): Result {
        if (attempt + 1 >= Outbox.MAX_ATTEMPTS) {
            // A body was sent at least once: it may have arrived (a read
            // timeout), so do not claim it never reached the bridge.
            val reason = if (frozen.get(job.key) != null) NotSent.UNCERTAIN else NotSent.UNREACHABLE
            frozen.remove(job.key)
            feedback.notSent(job, reason)
            return Result.success()
        }
        // A dialog's chain holds this job alone, so the next run can follow it
        // after the bridge's delay. A reply's chain may hold a later reply,
        // which an appended run would let go first: replies keep the backoff.
        if (job.request.action.answersDialog && afterSecs != null && afterSecs > Outbox.backoffSecs(runAttemptCount)) {
            Outbox.enqueueAfter(applicationContext, job, afterSecs, attempt + 1)
            return Result.success()
        }
        return Result.retry()
    }

    private companion object {
        const val TAG = "PassporterNotify"
    }
}
