package dev.passporter.notify.actions

import dev.passporter.notify.DEVICE
import android.Manifest
import android.app.Application
import android.app.KeyguardManager
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
import dev.passporter.notify.OTHER_DEVICE
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
import java.util.concurrent.Executor

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
        NoticeActionReceiver.executor = Executor { it.run() }
        pairedDevice = { DEVICE }
    }

    private fun show(notice: Notice) {
        manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, NoticeNotification.build(context, notice, 1, DEVICE))
    }

    private fun shown(): Notification? = shadowOf(manager).getNotification("w1:p1", NoticeNotification.ID)

    private fun messages(notification: Notification): List<String> =
        requireNotNull(NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(notification))
            .messages.map { "${it.person?.name ?: "you"}: ${it.text}" }

    private fun subText(notification: Notification) = notification.extras.getCharSequence(Notification.EXTRA_SUB_TEXT)?.toString()

    private fun action(index: Int) = requireNotNull(NotificationCompat.getAction(requireNotNull(shown()), index))

    private fun actionIntent(index: Int) = Intent(shadowOf(requireNotNull(action(index).actionIntent)).savedIntent)

    /** Taps action [index] of the shown notification, with [typed] text for a `RemoteInput`. */
    private fun tap(index: Int, typed: String? = null) {
        val action = action(index)
        val intent = actionIntent(index)
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
        queued().filter { it.state == WorkInfo.State.ENQUEUED }.forEach {
            driver.setInitialDelayMet(it.id)
            driver.setAllConstraintsMet(it.id)
        }
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
        assertEquals(listOf(info.id), WorkManager.getInstance(context).getWorkInfosByTag(Outbox.keyTag(key)).get().map { it.id })

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
        assertEquals("never fails a reply after it", WorkInfo.State.SUCCEEDED, queued().single().state)
        assertNull("an outcome, no longer a job", shown.extras.getString(ActionFeedback.EXTRA_JOB))
        assertTrue(shown.extras.getBoolean(ActionFeedback.EXTRA_OUTCOME))
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
        assertEquals(ListenableWorker.Result.success(), last.doWork())
        assertEquals("Couldn't reach the bridge: open the app", subText(requireNotNull(shown())))
        assertTrue(frozen.bodies.isEmpty())
    }

    @Test
    fun givingUpAfterABodyWasSentSaysItMayHaveArrived() {
        val job = job(NoticeAction.ALLOW_ONCE)
        show(notice())
        ActionFeedback(context).sending(job)
        frozen.put(KEY, "{}")
        val timeouts = FakeBridge { HttpResponse(504, "") }
        assertEquals(ListenableWorker.Result.success(), worker(timeouts, job, runs = Outbox.MAX_ATTEMPTS - 1).doWork())
        assertEquals("Not sure it arrived: open the app", subText(requireNotNull(shown())))
        assertTrue(frozen.bodies.isEmpty())
    }

    @Test
    fun aReplyKeepsTheBackoffWhateverTheRetryAfter() {
        val job = job(NoticeAction.REPLY, text = "go")
        val limited = FakeBridge.of(status = "done", dialog = "null", since = 0, action = HttpResponse(429, "", retryAfterSecs = 600))
        assertEquals(ListenableWorker.Result.retry(), worker(limited, job).doWork())
        assertTrue("nothing appended after a later reply", queued().isEmpty())
    }

    @Test
    fun aRetryAfterLongerThanTheBackoffIsFollowedInTheSameChain() {
        show(notice())
        tap(0)
        val later = Outbox.BACKOFF_SECS + 1
        var answers = listOf(HttpResponse(429, "", retryAfterSecs = later), HttpResponse(200, """{"status":"sent"}"""))
        val fake = FakeBridge { sent ->
            when (sent.path) {
                "/v1/actions" -> answers.first().also { answers = answers.drop(1) }
                "/v1/agents" -> HttpResponse(200, agents())
                else -> HttpResponse(200, PERMISSION)
            }
        }
        bridge = fake
        val first = runQueued()
        val chain = queued()
        assertEquals(setOf(WorkInfo.State.SUCCEEDED, WorkInfo.State.ENQUEUED), chain.map { it.state }.toSet())
        assertEquals(later * 1000, chain.single { it.state == WorkInfo.State.ENQUEUED }.initialDelayMillis)
        assertEquals("the body stays frozen for the next run", setOf(first.key), frozen.keys())
        assertEquals(first.key, runQueued().key)
        assertEquals(2, fake.actions.size)
        assertEquals("the same body twice", 1, fake.actions.distinct().size)
        assertNull("sent", shown())
    }

    @Test
    fun aRetryAfterIsCutAndAShorterOneIsLeftToTheBackoff() {
        val job = job(NoticeAction.ALLOW_ONCE)
        val forever = FakeBridge.of(action = HttpResponse(429, "", retryAfterSecs = 86_400))
        assertEquals(ListenableWorker.Result.success(), worker(forever, job).doWork())
        val next = WorkManager.getInstance(context).getWorkInfosForUniqueWork(Outbox.uniqueName(job.request)).get().single()
        assertEquals(Outbox.MAX_RETRY_AFTER_SECS * 1000, next.initialDelayMillis)
        val shorter = FakeBridge.of(action = HttpResponse(429, "", retryAfterSecs = Outbox.BACKOFF_SECS))
        assertEquals(ListenableWorker.Result.retry(), worker(shorter, job).doWork())
    }

    @Test
    fun aLockedPhoneAnswersNothing() {
        show(notice())
        shadowOf(context.getSystemService(KeyguardManager::class.java)).setIsDeviceLocked(true)
        tap(0)
        assertTrue(queued().isEmpty())
        val shown = requireNotNull(shown())
        assertEquals("Unlock your phone to answer", subText(shown))
        assertEquals("still answerable once unlocked", 3, NotificationCompat.getActionCount(shown))
    }

    @Test
    fun aKeyguardKeptOpenByATrustAgentStillCounts() {
        show(notice())
        // Smart Lock: not "device locked", but the keyguard shows.
        shadowOf(context.getSystemService(KeyguardManager::class.java)).setKeyguardLocked(true)
        tap(0)
        assertTrue(queued().isEmpty())
        assertEquals("Unlock your phone to answer", subText(requireNotNull(shown())))
    }

    @Test
    fun aSecondTapOnADialogQueuesNothing() {
        show(notice())
        val allow = actionIntent(0)
        val deny = actionIntent(1)
        NoticeActionReceiver().onReceive(context, allow)
        NoticeActionReceiver().onReceive(context, deny)
        val job = queued().single()
        bridge = FakeBridge.of()
        assertEquals(NoticeAction.ALLOW_ONCE, runQueued().request.action)
        assertEquals(job.id, queued().single().id)
    }

    @Test
    fun repliesToOneAgentGoOneAfterTheOther() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        val reply = actionIntent(0)
        val inputs = requireNotNull(action(0).remoteInputs)
        for (text in listOf("first", "second")) {
            val intent = Intent(reply)
            val results = Bundle().apply { putCharSequence(NoticeActions.KEY_TEXT, text) }
            RemoteInput.addResultsToIntent(inputs, intent, results)
            NoticeActionReceiver().onReceive(context, intent)
        }
        assertEquals(setOf(WorkInfo.State.BLOCKED, WorkInfo.State.ENQUEUED), queued().map { it.state }.toSet())
        bridge = FakeBridge.of(status = "done", dialog = "null", since = 0)
        runQueued()
        runQueued()
        assertEquals(listOf("first", "second"), ran.map { it.text })
    }

    @Test
    fun backoffDoubles() {
        assertEquals(listOf(30L, 60L, 120L, 240L), (0..3).map(Outbox::backoffSecs))
    }

    @Test
    fun aTapOnANoticeThatNewsReplacedQueuesNothing() {
        show(notice())
        val old = actionIntent(0)
        show(notice(line = "Edit main.rs?", pending = "screen-3-cd"))
        val news = requireNotNull(shown())
        NoticeActionReceiver().onReceive(context, old)
        assertTrue(queued().isEmpty())
        val shown = requireNotNull(shown())
        assertEquals(listOf("widget: Edit main.rs?"), messages(shown))
        assertEquals(3, NotificationCompat.getActionCount(shown))
        assertEquals(news.extras.getString(NoticeNotification.EXTRA_QUESTION), shown.extras.getString(NoticeNotification.EXTRA_QUESTION))
    }

    @Test
    fun anAnswerThatAlreadyLandedIsNotBroughtBackAsAlreadyAnswered() {
        val job = job(NoticeAction.ALLOW_ONCE)
        assertEquals(ListenableWorker.Result.success(), worker(FakeBridge.of(dialog = "null"), job).doWork())
        assertNull(shown())
    }

    @Test
    fun aTapSchedulesACheckForAnAnswerLostBeforeItWasQueued() {
        show(notice())
        tap(0)
        val check = WorkManager.getInstance(context).getWorkInfosForUniqueWork(Reconciler.LEFTOVERS_NAME).get().single()
        assertEquals(WorkInfo.State.ENQUEUED, check.state)
        assertTrue("past the tap's grace", check.initialDelayMillis > Reconciler.ANSWER_GRACE_MS)
    }

    @Test
    fun aTapMadeForAnotherPairingQueuesNothing() {
        show(notice())
        pairedDevice = { OTHER_DEVICE }
        tap(0)
        assertTrue(queued().isEmpty())
        val shown = requireNotNull(shown())
        assertEquals("Not paired: open the app", subText(shown))
        assertNull(shown.extras.getString(ActionFeedback.EXTRA_JOB))
    }

    /** Queued before a new pairing, run after it (the reset failed): dropped, and the old conversation stays gone. */
    @Test
    fun aJobOfAnOldPairingIsDroppedUnseen() {
        pairedDevice = { OTHER_DEVICE }
        val fake = FakeBridge.of()
        assertEquals(ListenableWorker.Result.success(), worker(fake, job(NoticeAction.REPLY, "hi")).doWork())
        assertTrue(fake.requests.isEmpty())
        assertNull(shown())
    }
}
