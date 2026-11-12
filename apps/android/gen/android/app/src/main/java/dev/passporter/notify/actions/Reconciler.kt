package dev.passporter.notify.actions

import android.app.NotificationManager
import android.content.Context
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import androidx.work.Constraints
import androidx.work.Data
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequest
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.Method
import dev.passporter.notify.AgentId
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.NotificationLock
import java.util.concurrent.TimeUnit

/** One agent's news notification on show: who, and why it was news. */
data class ShownNotice(
    /** The agent. */
    val agent: AgentId,
    /** [NoticeState.BLOCKED] or [NoticeState.FINISHED]. */
    val state: NoticeState,
    /** When it was posted, which tells it from a newer notice of the same agent. */
    val postedAt: Long,
)

/**
 * Clears notifications whose news is old (design §6.6): a dialog answered
 * on the desktop, a finished turn seen there. The bridge sends no
 * "resolved" message, so the app compares what it shows with the agent
 * list when it comes to the foreground and after an answer from the shade
 * was sent. A tap on an action also schedules a check past
 * [ANSWER_GRACE_MS] ([checkLeftoversLater]), which finds the tap's answer
 * if it was lost before it was queued.
 */
object Reconciler {
    /**
     * The agents of [shown] whose news no longer holds in [agents]: gone,
     * no longer blocked (for a dialog), or no longer unread (for a finished
     * turn). An agent herdr cannot tell about ([AgentStatus.UNKNOWN]) keeps
     * its notification. Pure.
     */
    fun stale(shown: List<ShownNotice>, agents: List<AgentState>): List<AgentId> {
        val byId = agents.associateBy { it.id }
        return shown.filter { notice ->
            val agent = byId[notice.agent]
            when {
                agent == null -> true
                agent.status == AgentStatus.UNKNOWN -> false
                notice.state == NoticeState.BLOCKED -> agent.status != AgentStatus.BLOCKED
                else -> !agent.unread
            }
        }.map { it.agent }
    }

    /**
     * Checks once a network is up, off the main thread. Cheap: a check
     * already queued is replaced, and with no notification on show it
     * ends without a request.
     */
    fun schedule(context: Context) {
        val request = OneTimeWorkRequest.Builder(ReconcileWorker::class.java)
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(UNIQUE_NAME, ExistingWorkPolicy.REPLACE, request)
    }

    /**
     * Checks once [ANSWER_GRACE_MS] (and a little) from now, network or
     * not, for leftovers only (news is compared with the agent list on
     * resume and after a sent answer, as before): an answer's notification
     * whose job is not queued by then was lost, and says so. A later tap moves the check later. Waits until
     * WorkManager has it, so call it before the tap's notification shows
     * it sending. Blocking.
     */
    fun checkLeftoversLater(context: Context) {
        val request = OneTimeWorkRequest.Builder(ReconcileWorker::class.java)
            .setInitialDelay(ANSWER_GRACE_MS + LEFTOVER_MARGIN_MS, TimeUnit.MILLISECONDS)
            .setInputData(Data.Builder().putBoolean(LEFTOVERS_ONLY, true).build())
            .build()
        try {
            WorkManager.getInstance(context)
                .enqueueUniqueWork(LEFTOVERS_NAME, ExistingWorkPolicy.REPLACE, request)
                .result.get()
        } catch (e: Exception) {
            // The next resume checks anyway.
            Log.w(TAG, "could not schedule a check for lost answers", e)
        }
    }

    /**
     * The news notifications on show. Those showing the progress of an
     * answer ([ActionFeedback.EXTRA_JOB], see [answers]) or its outcome
     * ([ActionFeedback.EXTRA_OUTCOME], left to the person) are not news.
     */
    fun shown(context: Context): List<ShownNotice> =
        context.getSystemService(NotificationManager::class.java).activeNotifications.mapNotNull { posted ->
            val notification = posted.notification
            val extras = notification.extras
            val answer = extras.containsKey(ActionFeedback.EXTRA_JOB) || extras.containsKey(ActionFeedback.EXTRA_OUTCOME)
            if (posted.id != NoticeNotification.ID || answer) {
                return@mapNotNull null
            }
            val agent = posted.tag?.let(AgentId::parse) ?: return@mapNotNull null
            val state = NoticeNotification.stateOf(notification.channelId) ?: return@mapNotNull null
            ShownNotice(agent, state, posted.postTime)
        }

    /**
     * The agents whose notification shows the progress of an answer
     * ([ActionFeedback.EXTRA_JOB]) posted before [before], with the job's
     * key: a leftover once that job is no longer queued (lost: a settled
     * job's notification is gone or shows its outcome).
     */
    fun answers(context: Context, before: Long): Map<AgentId, String> =
        context.getSystemService(NotificationManager::class.java).activeNotifications.mapNotNull { posted ->
            val key = posted.notification.extras.getString(ActionFeedback.EXTRA_JOB)
            val agent = posted.tag?.let(AgentId::parse)
            if (posted.id != NoticeNotification.ID || key == null || agent == null || posted.postTime >= before) {
                null
            } else {
                agent to key
            }
        }.toMap()

    /** The unique work name of the check. */
    const val UNIQUE_NAME = "notify-reconcile"

    /** The unique work name of the check after a tap ([checkLeftoversLater]). */
    const val LEFTOVERS_NAME = "notify-reconcile-leftovers"

    private const val LEFTOVER_MARGIN_MS = 10_000L

    /** Input of a check that only settles leftovers ([checkLeftoversLater]). */
    internal const val LEFTOVERS_ONLY = "leftoversOnly"

    private const val TAG = "PassporterNotify"

    /** How old an answer's notification must be to count as a leftover: past its tap's queueing. */
    const val ANSWER_GRACE_MS = 60_000L
}

/**
 * Runs [Reconciler]'s check. Best effort: any failure just ends it. First,
 * without the network, it settles what the outbox left behind: an answer's
 * notification whose job is no longer queued shows it was not sent
 * ([ActionFeedback.lost]), and frozen bodies of jobs that are gone are
 * dropped. Then it compares the news on show with the agent list.
 */
class ReconcileWorker internal constructor(
    context: Context,
    params: WorkerParameters,
    private val bridge: Bridge,
    private val frozen: FrozenBodies,
    private val isQueued: (String) -> Boolean,
    private val now: () -> Long,
) : Worker(context, params) {
    /** The constructor WorkManager calls. */
    constructor(context: Context, params: WorkerParameters) : this(
        context,
        params,
        DeviceBridge(context.applicationContext),
        PreferenceFrozenBodies(context),
        { key -> Outbox.isQueued(context, key) },
        System::currentTimeMillis,
    )

    private val manager = NotificationManagerCompat.from(applicationContext)

    override fun doWork(): Result {
        clearLeftovers()
        if (inputData.getBoolean(Reconciler.LEFTOVERS_ONLY, false)) return Result.success()
        val shown = Reconciler.shown(applicationContext)
        if (shown.isEmpty()) return Result.success()
        val response = try {
            bridge.send(Method.GET, "/v1/agents", null, null)
        } catch (e: DeviceAuthException) {
            Log.i(TAG, "no agent list to reconcile with: ${e.code}")
            return Result.success()
        }
        val agents = response.takeIf { it.isSuccess }?.let { BridgeState.agents(it.body) } ?: return Result.success()
        // Only what is still on show as it was: news posted while the list
        // was on its way stays.
        synchronized(NotificationLock) {
            val current = Reconciler.shown(applicationContext).toSet()
            for (agent in Reconciler.stale(shown.filter { it in current }, agents)) {
                manager.cancel(NoticeNotification.tag(agent), NoticeNotification.ID)
            }
        }
        return Result.success()
    }

    private fun clearLeftovers() {
        val before = now() - Reconciler.ANSWER_GRACE_MS
        val lost = Reconciler.answers(applicationContext, before).filterValues { !isQueued(it) }
        // Only those still showing the same lost job (ActionFeedback checks).
        val feedback = ActionFeedback(applicationContext)
        for ((agent, key) in lost) feedback.lost(agent, key)
        frozen.keys().filterNot(isQueued).forEach(frozen::remove)
    }

    private companion object {
        const val TAG = "PassporterNotify"
    }
}
