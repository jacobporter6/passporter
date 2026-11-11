package dev.passporter.notify.actions

import android.app.NotificationManager
import android.content.Context
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import androidx.work.Constraints
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
 * was sent.
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
     * The news notifications on show. Those showing the progress of an
     * answer ([ActionFeedback.EXTRA_JOB]) are left alone: the outbox
     * settles them.
     */
    fun shown(context: Context): List<ShownNotice> =
        context.getSystemService(NotificationManager::class.java).activeNotifications.mapNotNull { posted ->
            val notification = posted.notification
            if (posted.id != NoticeNotification.ID || notification.extras.containsKey(ActionFeedback.EXTRA_JOB)) {
                return@mapNotNull null
            }
            val agent = posted.tag?.let(AgentId::parse) ?: return@mapNotNull null
            val state = NoticeNotification.stateOf(notification.channelId) ?: return@mapNotNull null
            ShownNotice(agent, state, posted.postTime)
        }

    /** The unique work name of the check. */
    const val UNIQUE_NAME = "notify-reconcile"
}

/** Runs [Reconciler]'s check. Best effort: any failure just ends it. */
class ReconcileWorker internal constructor(
    context: Context,
    params: WorkerParameters,
    private val bridge: Bridge,
) : Worker(context, params) {
    /** The constructor WorkManager calls. */
    constructor(context: Context, params: WorkerParameters) :
        this(context, params, DeviceBridge(context.applicationContext))

    override fun doWork(): Result {
        val shown = Reconciler.shown(applicationContext)
        if (shown.isEmpty()) return Result.success()
        val response = try {
            bridge.send(Method.GET, "/v1/agents", null)
        } catch (e: DeviceAuthException) {
            Log.i(TAG, "no agent list to reconcile with: ${e.code}")
            return Result.success()
        }
        val agents = response.takeIf { it.isSuccess }?.let { BridgeState.agents(it.body) } ?: return Result.success()
        // Only what is still on show as it was: news posted while the list
        // was on its way stays.
        val current = Reconciler.shown(applicationContext).toSet()
        val manager = NotificationManagerCompat.from(applicationContext)
        for (agent in Reconciler.stale(shown.filter { it in current }, agents)) {
            manager.cancel(NoticeNotification.tag(agent), NoticeNotification.ID)
        }
        return Result.success()
    }

    private companion object {
        const val TAG = "PassporterNotify"
    }
}
