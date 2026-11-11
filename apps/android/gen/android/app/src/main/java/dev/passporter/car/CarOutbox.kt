package dev.passporter.car

import android.content.Context
import android.util.Log
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequest
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters
import dev.passporter.notify.actions.Bridge
import dev.passporter.notify.actions.Outbox
import java.util.concurrent.TimeUnit

/**
 * The car's outbox: each spoken reply is one WorkManager job, like an
 * answer from the shade ([Outbox]): it waits for a network, retries with
 * the same exponential backoff and gives up after the same
 * [Outbox.MAX_ATTEMPTS], or once the reply is older than [MAX_AGE_MS],
 * whichever comes first. Replies to one agent are appended, so they reach
 * it in the order they were said, and a job always ends in success so it
 * never fails the one after it.
 */
object CarOutbox {
    /**
     * The oldest reply still sent: about four retries. A reply to a turn
     * names no dialog, so the bridge cannot tell it was said to an older
     * one; past this it is not sent at all.
     */
    const val MAX_AGE_MS = 10 * 60_000L

    /** Tags every car job, for tests and diagnostics. */
    const val TAG = "dev.passporter.car.outbox"

    /** Queues [job], and waits until WorkManager has it. Blocking. */
    fun enqueue(context: Context, job: CarJob) {
        WorkManager.getInstance(context)
            .enqueueUniqueWork(uniqueName(job), ExistingWorkPolicy.APPEND_OR_REPLACE, request(job))
            .result.get()
    }

    /** The unique work name of [job]'s agent: its replies go one after the other. */
    fun uniqueName(job: CarJob): String = "car-reply:${job.request.agent.value}"

    /** The work request of [job]: network-bound, backing off exponentially. */
    fun request(job: CarJob): OneTimeWorkRequest =
        OneTimeWorkRequest.Builder(CarReplyWorker::class.java)
            .setInputData(CarJobData.write(job))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, Outbox.BACKOFF_SECS, TimeUnit.SECONDS)
            .addTag(TAG)
            .build()
}

/**
 * Runs one attempt of a [CarJob] ([CarReplySender]) and puts the answer
 * into the agent's conversation for the car to read aloud ([CarSpeech]).
 * A retry follows WorkManager's backoff; a `Retry-After` is not followed
 * beyond it, as for the shade's replies (an appended run could let a later
 * reply go first).
 */
class CarReplyWorker internal constructor(
    context: Context,
    params: WorkerParameters,
    private val bridge: Bridge,
    private val now: () -> Long = System::currentTimeMillis,
) : Worker(context, params) {
    /** The constructor WorkManager calls. */
    constructor(context: Context, params: WorkerParameters) :
        this(context, params, CarBridge(context.applicationContext))

    override fun doWork(): Result {
        val job = CarJobData.read(inputData) ?: run {
            Log.w(TAG, "dropped an unreadable car job")
            return Result.success()
        }
        val speech = CarSpeech(applicationContext)
        if (now() - job.heardAt > CarOutbox.MAX_AGE_MS) {
            speech.trouble(job.request, job.name, job.utterance, CarTrouble.TOO_OLD)
            return Result.success()
        }
        val outcome = CarReplySender(bridge).send(job)
        // Stopped meanwhile: WorkManager runs it again; nothing may show.
        if (isStopped) return Result.retry()
        when (outcome) {
            is CarOutcome.Spoken -> speech.answer(job, outcome.speech, outcome.sent)
            is CarOutcome.Trouble -> speech.trouble(job.request, job.name, job.utterance, outcome.trouble)
            is CarOutcome.Retry -> {
                if (runAttemptCount + 1 < Outbox.MAX_ATTEMPTS) return Result.retry()
                speech.trouble(job.request, job.name, job.utterance, CarTrouble.UNREACHABLE)
            }
        }
        return Result.success()
    }

    private companion object {
        const val TAG = "PassporterCar"
    }
}
