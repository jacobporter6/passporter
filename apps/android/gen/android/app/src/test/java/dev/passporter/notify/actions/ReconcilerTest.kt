package dev.passporter.notify.actions

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import android.content.Context
import androidx.work.ListenableWorker
import androidx.work.WorkerFactory
import androidx.work.WorkerParameters
import androidx.work.testing.TestListenableWorkerBuilder
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.agent
import dev.passporter.notify.notice
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

@RunWith(RobolectricTestRunner::class)
class ReconcilerTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val manager = context.getSystemService(NotificationManager::class.java)

    private fun state(id: String, status: AgentStatus, unread: Boolean = false) =
        AgentState(agent(id), status, seq = 1, statusSince = 0, unread = unread)

    private fun shown(id: String, state: NoticeState) = ShownNotice(agent(id), state, postedAt = 0)

    @Test
    fun aDialogIsStaleOnceTheAgentIsNoLongerBlocked() {
        val shown = listOf(shown("w1:p1", NoticeState.BLOCKED))
        assertEquals(emptyList<Any>(), Reconciler.stale(shown, listOf(state("w1:p1", AgentStatus.BLOCKED))))
        for (status in listOf(AgentStatus.WORKING, AgentStatus.IDLE, AgentStatus.DONE)) {
            assertEquals(listOf(agent("w1:p1")), Reconciler.stale(shown, listOf(state("w1:p1", status))))
        }
    }

    @Test
    fun anAgentHerdrCannotTellAboutKeepsItsNotification() {
        val shown = listOf(shown("w1:p1", NoticeState.BLOCKED), shown("w1:p2", NoticeState.FINISHED))
        val unknown = listOf(state("w1:p1", AgentStatus.UNKNOWN), state("w1:p2", AgentStatus.UNKNOWN))
        assertEquals(emptyList<Any>(), Reconciler.stale(shown, unknown))
    }

    @Test
    fun aFinishedTurnIsStaleOnceSeen() {
        val shown = listOf(shown("w1:p2", NoticeState.FINISHED))
        assertEquals(emptyList<Any>(), Reconciler.stale(shown, listOf(state("w1:p2", AgentStatus.DONE, unread = true))))
        assertEquals(listOf(agent("w1:p2")), Reconciler.stale(shown, listOf(state("w1:p2", AgentStatus.IDLE))))
        assertEquals(listOf(agent("w1:p2")), Reconciler.stale(shown, listOf(state("w1:p2", AgentStatus.WORKING))))
    }

    @Test
    fun aGoneAgentIsStale() {
        val shown = listOf(shown("w1:p1", NoticeState.BLOCKED), shown("w1:p2", NoticeState.FINISHED))
        assertEquals(listOf(agent("w1:p1"), agent("w1:p2")), Reconciler.stale(shown, emptyList()))
    }

    @Before
    fun grant() {
        shadowOf(context).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        NoticeNotification.createChannels(context)
    }

    private fun show(notice: Notice) {
        manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, NoticeNotification.build(context, notice, 1))
    }

    private fun run(bridge: Bridge) =
        TestListenableWorkerBuilder<ReconcileWorker>(context)
            .setWorkerFactory(object : WorkerFactory() {
                override fun createWorker(appContext: Context, workerClassName: String, workerParameters: WorkerParameters): ListenableWorker =
                    ReconcileWorker(appContext, workerParameters, bridge)
            })
            .build()
            .doWork()

    private fun tags() = shadowOf(manager).activeNotifications.map { it.tag }.toSet()

    @Test
    fun theWorkerCancelsOnlyOldNewsAndLeavesAnswersInProgress() {
        show(notice(agent = "w1:p1"))
        show(notice(agent = "w1:p2", state = NoticeState.FINISHED, pending = null))
        show(notice(agent = "w1:p3"))
        show(notice(agent = "w1:p4"))
        ActionFeedback(context).sending(
            OutboxJob(KEY, ActionRequest(NoticeAction.DENY, agent("w1:p4"), notice().pendingId), null, "widget", 0),
        )
        val list = """[
            {"id":"w1:p1","status":"blocked","seq":3,"statusSince":0},
            {"id":"w1:p2","status":"idle","seq":4,"statusSince":0},
            {"id":"w1:p4","status":"working","seq":5,"statusSince":0}]"""
        val bridge = FakeBridge { HttpResponse(200, list) }
        assertEquals(ListenableWorker.Result.success(), run(bridge))
        assertEquals(setOf("w1:p1", "w1:p4"), tags())
    }

    @Test
    fun withNothingShownItAsksNothing() {
        val bridge = FakeBridge { HttpResponse(200, "[]") }
        assertEquals(ListenableWorker.Result.success(), run(bridge))
        assertTrue(bridge.requests.isEmpty())
    }

    @Test
    fun withoutAnAgentListItKeepsEverything() {
        show(notice())
        assertEquals(ListenableWorker.Result.success(), run { _, _, _ -> throw DeviceAuthException.NotPaired() })
        assertEquals(ListenableWorker.Result.success(), run(FakeBridge { HttpResponse(502, "") }))
        assertEquals(setOf("w1:p1"), tags())
    }
}
