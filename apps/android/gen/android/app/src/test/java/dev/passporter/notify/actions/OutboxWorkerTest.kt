package dev.passporter.notify.actions

import android.Manifest
import android.app.Application
import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.os.Bundle
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import androidx.work.Configuration
import androidx.work.ListenableWorker
import androidx.work.NetworkType
import androidx.work.WorkInfo
import androidx.work.WorkManager
import androidx.work.WorkerFactory
import androidx.work.WorkerParameters
import androidx.work.testing.SynchronousExecutor
import androidx.work.testing.TestListenableWorkerBuilder
import androidx.work.testing.WorkManagerTestInitHelper
import dev.passporter.device.HttpResponse
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.agent
import dev.passporter.notify.notice
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

/** The receiver, the worker and the feedback in the notification, together. */
@RunWith(RobolectricTestRunner::class)
class OutboxWorkerTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val manager = context.getSystemService(NotificationManager::class.java)
    private val frozen = MemoryFrozen()

    /** The bridge the queued jobs meet. */
    private var bridge: Bridge = FakeBridge.of()

    /** The jobs WorkManager ran, in order. */
    private val ran = mutableListOf<OutboxJob>()

    private val factory = object : WorkerFactory() {
        override fun createWorker(appContext: Context, workerClassName: String, workerParameters: WorkerParameters): ListenableWorker? =
            when (workerClassName) {
                OutboxWorker::class.java.name -> {
                    OutboxData.read(workerParameters.inputData)?.let(ran::add)
                    OutboxWorker(appContext, workerParameters, bridge, frozen)
                }
                else -> null
            }
    }

    @Before
    fun setUp() {
        shadowOf(context).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        WorkManagerTestInitHelper.initializeTestWorkManager(
            context,
            Configuration.Builder().setExecutor(SynchronousExecutor()).setWorkerFactory(factory).build(),
        )
        NoticeNotification.createChannels(context)
    }

    private fun show(notice: Notice) {
        manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, NoticeNotification.build(context, notice, 1))
    }

    private fun shown(): Notification? = shadowOf(manager).getNotification("w1:p1", NoticeNotification.ID)

    private fun messages(notification: Notification): List<String> =
        requireNotNull(NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(notification))
            .messages.map { "${it.person?.name ?: "you"}: ${it.text}" }

    private fun subText(notification: Notification) = notification.extras.getCharSequence(Notification.EXTRA_SUB_TEXT)?.toString()

    /** Taps action [index] of the shown notification, with [typed] text for a `RemoteInput`. */
    private fun tap(index: Int, typed: String? = null) {
        val action = requireNotNull(NotificationCompat.getAction(requireNotNull(shown()), index))
        val intent = Intent(shadowOf(requireNotNull(action.actionIntent)).savedIntent)
        if (typed != null) {
            val results = Bundle().apply { putCharSequence(NoticeActions.KEY_TEXT, typed) }
            RemoteInput.addResultsToIntent(requireNotNull(action.remoteInputs), intent, results)
        }
        NoticeActionReceiver().onReceive(context, intent)
    }

    private fun queued(): List<WorkInfo> =
        WorkManager.getInstance(context).getWorkInfosByTag(Outbox.TAG).get()

    private fun worker(bridge: Bridge, job: OutboxJob, attempts: Int = 0, runs: Int = 0): OutboxWorker =
        TestListenableWorkerBuilder<OutboxWorker>(context)
            .setInputData(OutboxData.write(job, attempts))
            .setRunAttemptCount(runs)
            .setWorkerFactory(object : WorkerFactory() {
                override fun createWorker(appContext: Context, workerClassName: String, workerParameters: WorkerParameters): ListenableWorker =
                    OutboxWorker(appContext, workerParameters, bridge, frozen)
            })
            .build()

    /** Runs the queued outbox jobs as if a network came up; returns the last one. */
    private fun runQueued(): OutboxJob {
        val driver = requireNotNull(WorkManagerTestInitHelper.getTestDriver(context))
        queued().filter { it.state == WorkInfo.State.ENQUEUED }.forEach { driver.setAllConstraintsMet(it.id) }
        return ran.last()
    }

    @Test
    fun allowOnceIsQueuedAsANetworkBoundUniqueJobShownAsSendingThenSent() {
        show(notice())
        tap(0)
        val info = queued().single()
        assertEquals(WorkInfo.State.ENQUEUED, info.state)
        assertEquals(NetworkType.CONNECTED, info.constraints.requiredNetworkType)
        val sending = requireNotNull(shown())
        assertEquals(listOf("widget: Run `pnpm test`?", "you: Allow once"), messages(sending))
        assertEquals("Sending…", subText(sending))
        assertEquals("cannot be answered twice", 0, NotificationCompat.getActionCount(sending))
        assertTrue(sending.flags and Notification.FLAG_ONLY_ALERT_ONCE != 0)
        val key = requireNotNull(sending.extras.getString(ActionFeedback.EXTRA_JOB))
        assertTrue(key.matches(Regex("[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}")))
        assertEquals(info.id, WorkManager.getInstance(context).getWorkInfosForUniqueWork(Outbox.uniqueName(key)).get().single().id)

        val fake = FakeBridge.of()
        bridge = fake
        val job = runQueued()
        assertEquals(key, job.key)
        assertEquals(ActionRequest(NoticeAction.ALLOW_ONCE, agent("w1:p1"), job.request.pending), job.request)
        assertEquals("screen-2-ab", job.request.pending?.value)
        assertTrue(fake.actions.single().startsWith("{\"idempotencyKey\":\"$key\""))
        assertNull("settled", shown())
        assertEquals(WorkInfo.State.SUCCEEDED, queued().single().state)
    }

    @Test
    fun aReplyIsAppendedToTheConversationAndSentAsAPrompt() {
        show(notice(state = NoticeState.FINISHED, pending = null, line = "Fixed it"))
        tap(0, typed = "  now merge\r\n it\t ")
        assertEquals(listOf("widget: Fixed it", "you: now merge\n it"), messages(requireNotNull(shown())))
        val fake = FakeBridge.of(status = "done", dialog = "null", since = 0)
        bridge = fake
        val job = runQueued()
        assertEquals("now merge\n it", job.text)
        val body = fake.actions.single()
        assertTrue(body, body.contains("\"kind\":\"prompt\",\"payload\":{\"text\":\"now merge\\n it\"},\"origin\":\"notification\""))
        assertNull("settled", shown())
    }

    @Test
    fun anEmptyReplyLeavesTheNotificationAsItWas() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        tap(0, typed = " \n ")
        assertTrue(queued().isEmpty())
        val shown = requireNotNull(shown())
        assertEquals(1, NotificationCompat.getActionCount(shown))
        assertNull(subText(shown))
    }

    @Test
    fun aReplyTooLongForTheShadeSaysSoAndKeepsTheActions() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        tap(0, typed = "x".repeat(ReplyText.MAX_BYTES + 1))
        assertTrue(queued().isEmpty())
        val shown = requireNotNull(shown())
        assertEquals("Too long for a reply here: open the app", subText(shown))
        assertEquals(1, NotificationCompat.getActionCount(shown))
    }

    @Test
    fun aStaleAnswerSaysAlreadyAnswered() {
        show(notice())
        tap(0)
        val stale = FakeBridge.of(action = HttpResponse(409, """{"status":"stale"}"""))
        bridge = stale
        runQueued()
        val shown = requireNotNull(shown())
        assertEquals("Already answered", subText(shown))
        assertEquals(listOf("widget: Run `pnpm test`?", "you: Allow once"), messages(shown))
        assertEquals("never resent", 1, stale.actions.size)
        assertEquals(WorkInfo.State.FAILED, queued().single().state)
    }

    @Test
    fun newsThatReplacedTheNotificationStays() {
        show(notice())
        tap(0)
        show(notice(line = "Edit main.rs?", pending = "screen-3-cd"))
        val news = shown()
        bridge = FakeBridge.of()
        runQueued()
        assertEquals("a sent answer does not clear newer news", news, shown())
        val job = job(NoticeAction.ALLOW_ONCE)
        worker(FakeBridge.of(dialog = "null"), job).doWork()
        assertEquals("nor does one that was not sent", news, shown())
    }

    @Test
    fun noAnswerIsRetriedWithBackoffThenGivesUp() {
        val job = job(NoticeAction.ALLOW_ONCE)
        val offline = FakeBridge { HttpResponse(502, "") }
        assertEquals(ListenableWorker.Result.retry(), worker(offline, job).doWork())
        show(notice())
        ActionFeedback(context).sending(job)
        val last = worker(offline, job, attempts = 0, runs = Outbox.MAX_ATTEMPTS - 1)
        assertEquals(ListenableWorker.Result.failure(), last.doWork())
        assertEquals("Couldn't reach the bridge: open the app", subText(requireNotNull(shown())))
        assertTrue(frozen.bodies.isEmpty())
    }

    @Test
    fun aRetryAfterLongerThanTheBackoffIsFollowed() {
        val job = job(NoticeAction.ALLOW_ONCE)
        val limited = FakeBridge.of(action = HttpResponse(429, "", retryAfterSecs = Outbox.BACKOFF_SECS + 1))
        assertEquals(ListenableWorker.Result.success(), worker(limited, job).doWork())
        val next = WorkManager.getInstance(context).getWorkInfosForUniqueWork(Outbox.uniqueName(KEY)).get().single()
        assertEquals(WorkInfo.State.ENQUEUED, next.state)
        assertEquals("the body stays frozen for the next run", 1, frozen.bodies.size)
        val shorter = FakeBridge.of(action = HttpResponse(429, "", retryAfterSecs = Outbox.BACKOFF_SECS))
        assertEquals(ListenableWorker.Result.retry(), worker(shorter, job).doWork())
    }

    @Test
    fun backoffDoubles() {
        assertEquals(listOf(30L, 60L, 120L, 240L), (0..3).map(Outbox::backoffSecs))
    }
}
