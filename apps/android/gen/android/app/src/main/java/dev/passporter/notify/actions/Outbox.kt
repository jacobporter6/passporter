package dev.passporter.notify.actions

import android.content.Context
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.Data
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequest
import androidx.work.WorkManager
import dev.passporter.device.DeviceAuth
import dev.passporter.device.Scope
import dev.passporter.notify.AgentId
import dev.passporter.notify.PendingId
import java.util.concurrent.TimeUnit

/**
 * The native outbox (design §6.5): each answer from the shade is one
 * WorkManager job, unique by its idempotency key, that waits for a network
 * and retries with exponential backoff ([OutboxWorker]). WorkManager keeps
 * the job in its database, so it survives the app being killed or the
 * phone restarting.
 */
object Outbox {
    /** The first retry's delay; each next one doubles (WorkManager caps it at five hours). */
    const val BACKOFF_SECS = 30L

    /** Attempts before giving up: about two hours of an unreachable bridge, online. */
    const val MAX_ATTEMPTS = 8

    /** Queues [job]. A job already queued under its key is kept as it is. */
    fun enqueue(context: Context, job: OutboxJob) {
        WorkManager.getInstance(context).enqueueUniqueWork(uniqueName(job.key), ExistingWorkPolicy.KEEP, request(job))
    }

    /**
     * Queues [job]'s next attempt after [delaySecs], to follow the running
     * one (a bridge's `Retry-After` longer than the backoff). [attempts] is
     * how many it has had so far.
     */
    fun enqueueAfter(context: Context, job: OutboxJob, delaySecs: Long, attempts: Int) {
        WorkManager.getInstance(context).enqueueUniqueWork(
            uniqueName(job.key),
            ExistingWorkPolicy.APPEND_OR_REPLACE,
            request(job, delaySecs, attempts),
        )
    }

    /** The unique work name of the job keyed [key]. */
    fun uniqueName(key: String): String = "notify-answer:$key"

    /** The work request of [job]: network-bound, backing off exponentially. */
    fun request(job: OutboxJob, delaySecs: Long = 0, attempts: Int = 0): OneTimeWorkRequest =
        OneTimeWorkRequest.Builder(OutboxWorker::class.java)
            .setInputData(OutboxData.write(job, attempts))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, BACKOFF_SECS, TimeUnit.SECONDS)
            .setInitialDelay(delaySecs, TimeUnit.SECONDS)
            .addTag(TAG)
            .build()

    /** The delay WorkManager gives the retry after attempt number [attempt] (0 for the first). */
    fun backoffSecs(attempt: Int): Long = BACKOFF_SECS shl attempt.coerceIn(0, MAX_SHIFT)

    /** Tags every outbox job, for tests and diagnostics. */
    const val TAG = "dev.passporter.notify.outbox"

    /** Beyond this the five-hour cap applies anyway. */
    private const val MAX_SHIFT = 10
}

/** An [OutboxJob] as WorkManager input [Data]. */
internal object OutboxData {
    private const val KEY = "key"
    private const val ACTION = "action"
    private const val AGENT = "agent"
    private const val PENDING = "pending"
    private const val TEXT = "text"
    private const val NAME = "name"
    private const val CREATED_AT = "createdAt"
    private const val ATTEMPTS = "attempts"

    /** The input of [job], which has had [attempts] attempts. */
    fun write(job: OutboxJob, attempts: Int): Data =
        Data.Builder()
            .putString(KEY, job.key)
            .putString(ACTION, job.request.action.wire)
            .putString(AGENT, job.request.agent.value)
            .putString(PENDING, job.request.pending?.value)
            .putString(TEXT, job.text)
            .putString(NAME, job.name)
            .putLong(CREATED_AT, job.createdAt)
            .putInt(ATTEMPTS, attempts)
            .build()

    /** The job in [data], or `null` if it is not one (an older app's, say). */
    fun read(data: Data): OutboxJob? {
        val action = data.getString(ACTION)?.let(NoticeAction::fromWire) ?: return null
        val agent = data.getString(AGENT)?.let(AgentId::parse) ?: return null
        val pending = data.getString(PENDING)?.let { PendingId.parse(it) ?: return null }
        if ((pending != null) != action.answersDialog) return null
        val text = data.getString(TEXT)
        if ((text != null) != action.takesText) return null
        return OutboxJob(
            key = data.getString(KEY) ?: return null,
            request = ActionRequest(action, agent, pending),
            text = text,
            name = data.getString(NAME) ?: return null,
            createdAt = data.getLong(CREATED_AT, 0),
        )
    }

    /** Attempts made before this run's ([read]'s job). */
    fun attempts(data: Data): Int = data.getInt(ATTEMPTS, 0)
}

/** [FrozenBodies] in app-private preferences (never backed up). */
internal class PreferenceFrozenBodies(context: Context) : FrozenBodies {
    private val preferences = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    override fun get(key: String): String? = preferences.getString(key, null)

    // Committed before the request goes, so a crash cannot lose it.
    override fun put(key: String, body: String) {
        preferences.edit().putString(key, body).commit()
    }

    override fun remove(key: String) {
        preferences.edit().remove(key).apply()
    }

    private companion object {
        const val FILE = "dev.passporter.notify.outbox"
    }
}

/** [Bridge] over the app's device identity, with an app-scope session. */
internal class DeviceBridge(private val context: Context) : Bridge {
    override fun send(method: dev.passporter.device.Method, path: String, json: String?) =
        DeviceAuth.get(context).send(Scope.APP, method, path, json)
}
