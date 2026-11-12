package dev.passporter.notify.actions

import android.Manifest
import android.app.Application
import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import androidx.work.Data
import androidx.work.ListenableWorker
import androidx.work.WorkerFactory
import androidx.work.WorkerParameters
import androidx.work.testing.TestListenableWorkerBuilder
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.notify.DEVICE
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.agent
import dev.passporter.notify.notice
import dev.passporter.app.R
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
        manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, NoticeNotification.build(context, notice, 1, DEVICE))
    }

    private val frozen = MemoryFrozen()

    /**
     * Runs the check against [bridge], with the outbox's jobs [queued] and
     * the clock at [now] (by default, every notification is past its grace).
     */
    private fun run(bridge: Bridge, queued: Set<String> = setOf(KEY), now: Long = Long.MAX_VALUE / 2, input: Data = Data.EMPTY) =
        TestListenableWorkerBuilder<ReconcileWorker>(context)
            .setInputData(input)
            .setWorkerFactory(object : WorkerFactory() {
                override fun createWorker(appContext: Context, workerClassName: String, workerParameters: WorkerParameters): ListenableWorker =
                    ReconcileWorker(appContext, workerParameters, bridge, frozen, { it in queued }, { now })
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
            OutboxJob(KEY, ActionRequest(NoticeAction.DENY, agent("w1:p4"), notice().pendingId), null, "widget", 0, DEVICE),
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
        assertEquals(ListenableWorker.Result.success(), run(Bridge { _, _, _, _ -> throw DeviceAuthException.NotPaired() }))
        assertEquals(ListenableWorker.Result.success(), run(FakeBridge { HttpResponse(502, "") }))
        assertEquals(setOf("w1:p1"), tags())
    }

    @Test
    fun anAnswerWhoseJobIsGoneSaysItWasNotSentAndItsFrozenBodyIsPruned() {
        show(notice())
        ActionFeedback(context).sending(job(NoticeAction.ALLOW_ONCE))
        frozen.put(KEY, "{}")
        frozen.put("other", "{}")
        val bridge = FakeBridge { HttpResponse(200, "[]") }
        run(bridge, queued = setOf(KEY), now = 0)
        assertEquals("still queued, and inside its grace", setOf("w1:p1"), tags())
        assertEquals(setOf(KEY), frozen.keys())
        run(bridge, queued = emptySet(), now = 0)
        assertEquals("a tap still being queued is left alone", setOf("w1:p1"), tags())
        run(bridge, queued = emptySet())
        // Lost before it was queued: the person must answer again, so it is not cleared as if sent.
        val lost = requireNotNull(shadowOf(manager).getNotification("w1:p1", NoticeNotification.ID))
        assertEquals(context.getString(R.string.notify_not_sent_failed), lost.extras.getCharSequence(Notification.EXTRA_SUB_TEXT)?.toString())
        assertTrue("an outcome, left to the person", lost.extras.getBoolean(ActionFeedback.EXTRA_OUTCOME))
        assertFalse(lost.extras.containsKey(ActionFeedback.EXTRA_JOB))
        assertEquals(emptySet<String>(), frozen.keys())
        assertTrue("no news on show: no request", bridge.requests.isEmpty())
        run(bridge, queued = emptySet())
        assertEquals("still shown", setOf("w1:p1"), tags())
    }

    @Test
    fun anOutcomeStaysUntilThePersonSeesIt() {
        show(notice())
        val job = job(NoticeAction.ALLOW_ONCE)
        ActionFeedback(context).sending(job)
        ActionFeedback(context).notSent(job, NotSent.UNCERTAIN)
        // The agent moved on, and the job is long gone: still shown.
        run(FakeBridge { HttpResponse(200, "[]") }, queued = emptySet())
        assertEquals(setOf("w1:p1"), tags())
    }

    /** The check after a tap settles leftovers only: news is left to the resume and a sent answer. */
    @Test
    fun theCheckAfterATapLeavesTheNewsAlone() {
        show(notice())
        val bridge = FakeBridge { HttpResponse(200, "[]") }
        run(bridge, input = Data.Builder().putBoolean(Reconciler.LEFTOVERS_ONLY, true).build())
        assertTrue(bridge.requests.isEmpty())
        assertEquals(setOf("w1:p1"), tags())
    }
}
