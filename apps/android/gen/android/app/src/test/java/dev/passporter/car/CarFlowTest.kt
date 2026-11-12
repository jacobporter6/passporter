package dev.passporter.car

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
import dev.passporter.notify.DEVICE
import dev.passporter.notify.Notice
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NoticeState
import dev.passporter.notify.OTHER_DEVICE
import dev.passporter.notify.actions.Bridge
import dev.passporter.notify.actions.FakeBridge
import dev.passporter.notify.actions.NoticeActionReceiver
import dev.passporter.notify.actions.NoticeActions
import dev.passporter.app.R
import dev.passporter.notify.PendingId
import dev.passporter.notify.agent
import dev.passporter.notify.actions.ActionFeedback
import dev.passporter.notify.actions.ActionRequest
import dev.passporter.notify.actions.NoticeAction
import dev.passporter.notify.actions.NotSent
import dev.passporter.notify.actions.Outbox
import dev.passporter.notify.actions.OutboxJob
import dev.passporter.notify.actions.PairedAs
import dev.passporter.notify.actions.Reconciler
import dev.passporter.notify.actions.currentDevice
import dev.passporter.notify.actions.pairedDevice
import dev.passporter.notify.notice
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import java.util.concurrent.Executor

/** The car receiver, its outbox and the spoken answer in the conversation, together. */
@RunWith(RobolectricTestRunner::class)
class CarFlowTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val manager = context.getSystemService(NotificationManager::class.java)

    /** Whether Android Auto projects, as the receiver sees it. */
    private var projecting = true

    /** The bridge the queued jobs meet. */
    private var bridge: Bridge = FakeBridge { HttpResponse(200, carReply("Sent.")) }

    private val factory = object : WorkerFactory() {
        override fun createWorker(appContext: Context, workerClassName: String, workerParameters: WorkerParameters): ListenableWorker? =
            when (workerClassName) {
                CarReplyWorker::class.java.name -> CarReplyWorker(appContext, workerParameters, bridge)
                else -> null
            }
    }

    @After
    fun restore() {
        pairedDevice = ::currentDevice
    }

    @Before
    fun setUp() {
        shadowOf(context).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        WorkManagerTestInitHelper.initializeTestWorkManager(
            context,
            Configuration.Builder().setExecutor(SynchronousExecutor()).setWorkerFactory(factory).build(),
        )
        NoticeNotification.createChannels(context)
        CarReplyReceiver.executor = Executor { it.run() }
        carProjection = { CarProjection { projecting } }
        NoticeActionReceiver.executor = Executor { it.run() }
        pairedDevice = { PairedAs.Device(DEVICE) }
    }

    private fun show(notice: Notice, now: Long = 1) {
        manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, NoticeNotification.build(context, notice, now, DEVICE))
    }

    private fun subText(notification: Notification) = notification.extras.getCharSequence(Notification.EXTRA_SUB_TEXT)?.toString()

    private fun shown(): Notification? = shadowOf(manager).getNotification("w1:p1", NoticeNotification.ID)

    private fun messages(notification: Notification): List<String> =
        requireNotNull(NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(notification))
            .messages.map { "${it.person?.name ?: "you"}: ${it.text}" }

    private fun car(notification: Notification, action: CarAction) =
        NotificationCompat.getInvisibleActions(notification)[action.ordinal]

    /** What Android Auto does: fills the reply's `RemoteInput` with [heard] and fires it, or fires mark-as-read. */
    private fun fire(action: CarAction, heard: String? = null) {
        val carAction = car(requireNotNull(shown()), action)
        val intent = Intent(shadowOf(requireNotNull(carAction.actionIntent)).savedIntent)
        if (heard != null) {
            val results = Bundle().apply { putCharSequence(CarActions.KEY_UTTERANCE, heard) }
            RemoteInput.addResultsToIntent(requireNotNull(carAction.remoteInputs), intent, results)
        }
        CarReplyReceiver().onReceive(context, intent)
    }

    private fun queued(): List<WorkInfo> = WorkManager.getInstance(context).getWorkInfosByTag(CarOutbox.TAG).get()

    /** Runs the queued car jobs as if a network came up. */
    private fun runQueued() {
        val driver = requireNotNull(WorkManagerTestInitHelper.getTestDriver(context))
        queued().filter { it.state == WorkInfo.State.ENQUEUED }.forEach { driver.setAllConstraintsMet(it.id) }
    }

    private fun worker(job: CarJob, bridge: Bridge, runs: Int = 0, now: Long = System.currentTimeMillis()): CarReplyWorker =
        TestListenableWorkerBuilder<CarReplyWorker>(context)
            .setInputData(CarJobData.write(job))
            .setRunAttemptCount(runs)
            .setWorkerFactory(object : WorkerFactory() {
                override fun createWorker(appContext: Context, workerClassName: String, workerParameters: WorkerParameters): ListenableWorker =
                    CarReplyWorker(appContext, workerParameters, bridge) { now }
            })
            .build()

    private fun lock() {
        val keyguard = shadowOf(context.getSystemService(KeyguardManager::class.java))
        keyguard.setIsDeviceLocked(true)
        keyguard.setKeyguardLocked(true)
    }

    @Test
    fun aYesInTheCarWithThePhoneLockedGoesToCarReplyAndIsReadBack() {
        show(notice())
        lock()
        fire(CarAction.REPLY, heard = "yes")
        val info = queued().single()
        assertEquals(NetworkType.CONNECTED, info.constraints.requiredNetworkType)
        val fake = FakeBridge { HttpResponse(200, carReply("Allowed: pnpm test")) }
        bridge = fake
        runQueued()
        val body = JSONObject(requireNotNull(fake.requests.single().body))
        assertEquals("/v1/agents/w1:p1/car-reply", fake.requests.single().path)
        assertEquals("yes", body.getString("utterance"))
        assertEquals("screen-2-ab", body.getString("pendingId"))
        assertTrue(body.getString("idempotencyKey").matches(Regex("[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}")))
        assertEquals("nothing else in the body", 3, body.length())

        val answered = requireNotNull(shown())
        assertEquals(listOf("widget: Run `pnpm test`?", "you: yes", "Passporter: Allowed: pnpm test"), messages(answered))
        assertFalse("a new message the car announces", answered.flags and Notification.FLAG_ONLY_ALERT_ONCE != 0)
        assertEquals("the dialog is answered: no shade actions", 0, NotificationCompat.getActionCount(answered))
        assertEquals("the car can still reply", 2, NotificationCompat.getInvisibleActions(answered).size)
        assertEquals("screen-2-ab", answered.extras.getString(NoticeNotification.EXTRA_QUESTION))
    }

    @Test
    fun theUtteranceIsSentAsHeardOnOneLine() {
        show(notice(state = NoticeState.FINISHED, pending = null, decide = "Merge now?"))
        fire(CarAction.REPLY, heard = " no,\nuse the ​other file ")
        val fake = FakeBridge { HttpResponse(200, carReply("Sent.")) }
        bridge = fake
        runQueued()
        val body = JSONObject(requireNotNull(fake.requests.single().body))
        assertEquals("no, use the other file", body.getString("utterance"))
        assertFalse("a finished turn names no dialog", body.has("pendingId"))
    }

    @Test
    fun anAmbiguousAnswerIsReadBackAndTheDialogStaysAnswerable() {
        show(notice())
        fire(CarAction.REPLY, heard = "yes but run the tests first")
        val ambiguous = "I'm not sure what you meant. Say just yes to allow, or no and what to do instead."
        bridge = FakeBridge { HttpResponse(403, carReply(ambiguous, status = "refused")) }
        runQueued()
        val shown = requireNotNull(shown())
        assertEquals(
            listOf("widget: Run `pnpm test`?", "you: yes but run the tests first", "Passporter: $ambiguous"),
            messages(shown),
        )
        assertEquals("the shade can still answer", 3, NotificationCompat.getActionCount(shown))
        assertEquals(2, NotificationCompat.getInvisibleActions(shown).size)
        assertEquals("never retried", WorkInfo.State.SUCCEEDED, queued().single().state)
    }

    @Test
    fun offTheCarTheCarActionsSendNothingLockedOrNotAndSaySo() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        val before = requireNotNull(shown())
        projecting = false
        fire(CarAction.MARK_AS_READ)
        assertEquals("mark-as-read does nothing", before, shown())
        fire(CarAction.REPLY, heard = "merge it")
        lock()
        fire(CarAction.REPLY, heard = "merge it")
        assertTrue(queued().isEmpty())
        val after = requireNotNull(shown())
        assertEquals("Answer from your phone", subText(after))
        assertEquals("nothing added to the conversation", messages(before), messages(after))
        assertTrue(after.flags and Notification.FLAG_ONLY_ALERT_ONCE != 0)
        assertEquals("still answerable", 1, NotificationCompat.getActionCount(after))
        assertEquals(2, NotificationCompat.getInvisibleActions(after).size)
        assertEquals(1L, after.extras.getLong(NoticeNotification.EXTRA_STAMP))
    }

    @Test
    fun aLateAnswerToAnOlderTurnDoesNotTouchANewerUnannouncedOne() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        fire(CarAction.REPLY, heard = "merge it")
        val quiet = notice(state = NoticeState.FINISHED, pending = null, line = "Ran the tests", announce = false)
        show(quiet, now = 2)
        val news = shown()
        bridge = FakeBridge { HttpResponse(200, carReply("Sent.")) }
        runQueued()
        assertEquals(news, shown())
        assertTrue("still nothing for the car", NotificationCompat.getInvisibleActions(requireNotNull(shown())).isEmpty())
    }

    @Test
    fun aGoneConversationComesBackOnlyWhileTheCarProjects() {
        show(notice(state = NoticeState.FINISHED, pending = null, decide = "Merge now?"))
        fire(CarAction.REPLY, heard = "yes")
        manager.cancelAll()
        projecting = false
        runQueued()
        assertNull("off the car, cleared news stays cleared", shown())

        projecting = true
        CarSpeech(context).answer(carJob("yes", pending = null), "Sent.", sent = true)
        val back = requireNotNull(shown())
        assertEquals(listOf("you: yes", "Passporter: Sent."), messages(back))
        assertEquals(2, NotificationCompat.getInvisibleActions(back).size)
        assertEquals("turn", back.extras.getString(NoticeNotification.EXTRA_QUESTION))
    }

    @Test
    fun aReplyTooOldIsNotSent() {
        show(notice())
        val job = carJob(heardAt = 1_000_000)
        val fake = FakeBridge { HttpResponse(200, carReply("Sent.")) }
        val late = worker(job, fake, now = 1_000_000 + CarOutbox.MAX_AGE_MS + 1)
        assertEquals(ListenableWorker.Result.success(), late.doWork())
        assertTrue(fake.requests.isEmpty())
        assertEquals(
            "Passporter: I couldn't reach your computer in time, so I stopped trying. Please check your phone.",
            messages(requireNotNull(shown())).last(),
        )
        assertEquals(ListenableWorker.Result.success(), worker(job, fake, now = 1_000_000 + CarOutbox.MAX_AGE_MS).doWork())
        assertEquals(1, fake.requests.size)
    }

    @Test
    fun theShadeStillNeedsAnUnlockedPhoneWhileTheCarProjects() {
        show(notice())
        lock()
        val allow = requireNotNull(NotificationCompat.getAction(requireNotNull(shown()), 0))
        NoticeActionReceiver().onReceive(context, Intent(shadowOf(requireNotNull(allow.actionIntent)).savedIntent))
        assertTrue(WorkManager.getInstance(context).getWorkInfosByTag(Outbox.TAG).get().isEmpty())
        assertTrue(queued().isEmpty())
    }

    @Test
    fun nothingHeardIsSaidSoWithoutAsking() {
        show(notice())
        fire(CarAction.REPLY, heard = " ​ ")
        assertTrue(queued().isEmpty())
        assertEquals("Passporter: I didn't catch that. Please say it again.", messages(requireNotNull(shown())).last())
        fire(CarAction.REPLY, heard = "x".repeat(CarReplyReceiver.MAX_UTTERANCE_BYTES + 1))
        assertTrue(queued().isEmpty())
        assertEquals(
            "Passporter: That was too long to send from the car. Please use your phone.",
            messages(requireNotNull(shown())).last(),
        )
    }

    @Test
    fun anUnreachableBridgeIsRetriedWithTheSameBodyThenGivesUpAloud() {
        show(notice())
        val job = carJob()
        val offline = FakeBridge { HttpResponse(503, carReply("I can't reach the agent right now.", status = "failed")) }
        assertEquals(ListenableWorker.Result.retry(), worker(job, offline).doWork())
        assertEquals(ListenableWorker.Result.retry(), worker(job, offline, runs = 1).doWork())
        assertEquals("the same key and body every time", 1, offline.requests.map { it.body }.distinct().size)
        assertEquals(ListenableWorker.Result.success(), worker(job, offline, runs = Outbox.MAX_ATTEMPTS - 1).doWork())
        assertEquals(
            "Passporter: I couldn't reach your computer, so that may not have gone through. Please check your phone.",
            messages(requireNotNull(shown())).last(),
        )
    }

    @Test
    fun repliesToOneAgentGoInTheOrderTheyWereSaid() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        fire(CarAction.REPLY, heard = "first")
        fire(CarAction.REPLY, heard = "second")
        assertEquals(setOf(WorkInfo.State.BLOCKED, WorkInfo.State.ENQUEUED), queued().map { it.state }.toSet())
        val fake = FakeBridge { HttpResponse(200, carReply("Sent.")) }
        bridge = fake
        runQueued()
        runQueued()
        assertEquals(listOf("first", "second"), fake.requests.map { JSONObject(requireNotNull(it.body)).getString("utterance") })
        val keys = fake.requests.map { JSONObject(requireNotNull(it.body)).getString("idempotencyKey") }
        assertEquals("a key per utterance", 2, keys.distinct().size)
    }

    @Test
    fun newsThatReplacedTheConversationIsNotOverwrittenByTheAnswer() {
        show(notice())
        fire(CarAction.REPLY, heard = "yes")
        show(notice(line = "Edit main.rs?", pending = "screen-3-cd"))
        val news = shown()
        bridge = FakeBridge { HttpResponse(409, carReply("The agent has moved on. Please check your phone.", status = "stale")) }
        runQueued()
        assertEquals(news, shown())
    }

    @Test
    fun markAsReadLeavesTheConversationForTheCarToReadAndReplyTo() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        val before = shown()
        fire(CarAction.MARK_AS_READ)
        assertEquals(before, shown())
        fire(CarAction.REPLY, heard = "merge it")
        assertEquals(1, queued().size)
    }

    @Test
    fun aReplyThatCannotBeSentComesBackQuietlyOffTheCarEvenIfTheConversationWent() {
        show(notice(state = NoticeState.FINISHED, pending = null))
        manager.cancelAll()
        projecting = false
        val job = carJob("merge it", pending = null, heardAt = 1_000_000)
        worker(job, FakeBridge { HttpResponse(200, carReply("Sent.")) }, now = 1_000_000 + CarOutbox.MAX_AGE_MS + 1).doWork()
        val back = requireNotNull(shown())
        assertEquals(
            listOf("you: merge it", "Passporter: I couldn't reach your computer in time, so I stopped trying. Please check your phone."),
            messages(back),
        )
        assertTrue("quietly", back.flags and Notification.FLAG_ONLY_ALERT_ONCE != 0)
    }

    @Test
    fun aRefusalComesBackQuietlyAfterTheDriveButASentAnswerDoesNot() {
        show(notice())
        manager.cancelAll()
        projecting = false
        CarSpeech(context).answer(carJob("yes"), "Allowed: pnpm test", sent = true)
        assertNull(shown())
        CarSpeech(context).answer(carJob("yes"), "The agent has moved on. Please check your phone.", sent = false)
        val back = requireNotNull(shown())
        assertEquals("Passporter: The agent has moved on. Please check your phone.", messages(back).last())
        assertTrue("quietly", back.flags and Notification.FLAG_ONLY_ALERT_ONCE != 0)
    }

    @Test
    fun theTitleIsTheNotificationsWhateverTheIntentSays() {
        show(notice())
        val reply = car(requireNotNull(shown()), CarAction.REPLY)
        val intent = Intent(shadowOf(requireNotNull(reply.actionIntent)).savedIntent).putExtra(CarActions.EXTRA_NAME, "bank")
        val results = Bundle().apply { putCharSequence(CarActions.KEY_UTTERANCE, "yes") }
        RemoteInput.addResultsToIntent(requireNotNull(reply.remoteInputs), intent, results)
        CarReplyReceiver().onReceive(context, intent)
        bridge = FakeBridge { HttpResponse(200, carReply("Allowed: pnpm test")) }
        runQueued()
        val shown = requireNotNull(shown())
        assertEquals("widget", shown.extras.getString(NoticeNotification.EXTRA_AGENT_NAME))
        assertFalse(shown.extras.keySet().any { "${shown.extras.get(it)}".contains("bank") })
    }

    @Test
    fun aShadeRefusalKeepsTheCarActions() {
        show(notice())
        lock()
        val allow = requireNotNull(NotificationCompat.getAction(requireNotNull(shown()), 0))
        NoticeActionReceiver().onReceive(context, Intent(shadowOf(requireNotNull(allow.actionIntent)).savedIntent))
        assertEquals(2, NotificationCompat.getInvisibleActions(requireNotNull(shown())).size)
    }

    @Test
    fun anIntentThatIsNotACarActionIsIgnored() {
        show(notice())
        val before = shown()
        val shade = requireNotNull(NotificationCompat.getAction(requireNotNull(before), 2))
        val forged = Intent(shadowOf(requireNotNull(shade.actionIntent)).savedIntent).setAction(CarActions.INTENT_ACTION)
        RemoteInput.addResultsToIntent(
            requireNotNull(shade.remoteInputs),
            forged,
            Bundle().apply { putCharSequence(NoticeActions.KEY_TEXT, "yes") },
        )
        CarReplyReceiver().onReceive(context, forged)
        assertTrue(queued().isEmpty())
        assertEquals(before, shown())
    }

    @Test
    fun aCarReplyMadeForAnotherPairingSendsNothing() {
        show(notice())
        pairedDevice = { PairedAs.Device(OTHER_DEVICE) }
        fire(CarAction.REPLY, heard = "yes")
        assertTrue(queued().isEmpty())
        assertEquals(context.getString(R.string.car_trouble_not_paired), messages(requireNotNull(shown())).last().removePrefix("Passporter: "))
    }

    /** The shade's answer to the same dialog, tapped while the car's reply was queued. */
    private fun shadeAnswer(): OutboxJob {
        val allow = requireNotNull(NotificationCompat.getAction(requireNotNull(shown()), 0))
        NoticeActionReceiver().onReceive(context, Intent(shadowOf(requireNotNull(allow.actionIntent)).savedIntent))
        val key = requireNotNull(requireNotNull(shown()).extras.getString(ActionFeedback.EXTRA_JOB))
        val request = ActionRequest(NoticeAction.ALLOW_ONCE, agent("w1:p1"), PendingId.parse("screen-2-ab"))
        return OutboxJob(key, request, null, "widget", 0, DEVICE)
    }

    @Test
    fun theShadesAnswerSettlingNeverWipesWhatTheDriverMustHear() {
        show(notice())
        fire(CarAction.REPLY, heard = "no")
        val shade = shadeAnswer()
        bridge = FakeBridge { HttpResponse(200, carReply("Denied.")) }
        runQueued()
        val spoken = requireNotNull(shown())
        assertFalse("the car's turn no longer shows the shade's answer on its way", spoken.extras.containsKey(ActionFeedback.EXTRA_JOB))
        assertEquals(shade.key, spoken.extras.getString(ActionFeedback.EXTRA_SPOKEN_OVER))
        ActionFeedback(context).sent(shade)
        val after = requireNotNull(shown())
        assertEquals(messages(spoken), messages(after))
        assertEquals("Passporter: Denied.", messages(after).last())
        assertFalse("settled: never found lost later", after.extras.containsKey(ActionFeedback.EXTRA_SPOKEN_OVER))
        assertTrue(Reconciler.answers(context, Long.MAX_VALUE).isEmpty())
    }

    /** The shade's answer fails after the car spoke: the reason comes under the car's words, never silently lost. */
    @Test
    fun theShadesAnswerFailingAfterTheCarSpokeSaysSoUnderTheCarsWords() {
        show(notice())
        fire(CarAction.REPLY, heard = "no")
        val shade = shadeAnswer()
        bridge = FakeBridge { HttpResponse(200, carReply("Denied.")) }
        runQueued()
        ActionFeedback(context).notSent(shade, NotSent.UNREACHABLE)
        val after = requireNotNull(shown())
        val said = messages(after)
        assertEquals("Passporter: Denied.", said[said.size - 2])
        assertEquals("the reason reads as the shade's answer's", "you: ${context.getString(R.string.notify_action_allow_once)}", said.last())
        assertEquals(context.getString(R.string.notify_not_sent_unreachable), subText(after))
        assertTrue(after.extras.getBoolean(ActionFeedback.EXTRA_OUTCOME))
        assertEquals("the car can still take it", 2, NotificationCompat.getInvisibleActions(after).size)
    }

    /** The shade's answer lost before it was queued, after the car spoke: the reconciler still finds it. */
    @Test
    fun theShadesAnswerLostAfterTheCarSpokeIsStillFound() {
        show(notice())
        fire(CarAction.REPLY, heard = "no")
        val shade = shadeAnswer()
        bridge = FakeBridge { HttpResponse(200, carReply("Denied.")) }
        runQueued()
        assertEquals(mapOf(agent("w1:p1") to shade.key), Reconciler.answers(context, Long.MAX_VALUE))
        assertTrue("not news", Reconciler.shown(context).isEmpty())
        ActionFeedback(context).lost(agent("w1:p1"), shade.key)
        val after = requireNotNull(shown())
        val said = messages(after)
        assertEquals("Passporter: Denied.", said[said.size - 2])
        assertEquals("the reason reads as the shade's answer's", "you: ${context.getString(R.string.notify_your_shade_answer)}", said.last())
        assertEquals(context.getString(R.string.notify_not_sent_failed), subText(after))
    }

    @Test
    fun anOutcomeKeepsTheCarActionsAndStaysOutOfTheNewsAfterTheCarSpeaks() {
        show(notice())
        fire(CarAction.REPLY, heard = "no")
        val shade = shadeAnswer()
        ActionFeedback(context).notSent(shade, NotSent.UNCERTAIN)
        assertEquals("the car can still take it", 2, NotificationCompat.getInvisibleActions(requireNotNull(shown())).size)
        bridge = FakeBridge { HttpResponse(200, carReply("Denied.")) }
        runQueued()
        val spoken = requireNotNull(shown())
        assertEquals("Passporter: Denied.", messages(spoken).last())
        assertTrue(spoken.extras.getBoolean(ActionFeedback.EXTRA_OUTCOME))
        assertTrue("not news for the reconciler", Reconciler.shown(context).isEmpty())
    }

    /** Off the car, a car action says nothing: the shade's answer on its way keeps its mark, and its Sent clears the notification. */
    @Test
    fun offTheCarTheShadesAnswerStaysItsOwn() {
        show(notice())
        val shade = shadeAnswer()
        projecting = false
        fire(CarAction.REPLY, heard = "no")
        val after = requireNotNull(shown())
        assertEquals(shade.key, after.extras.getString(ActionFeedback.EXTRA_JOB))
        assertFalse(after.extras.containsKey(ActionFeedback.EXTRA_SPOKEN_OVER))
        ActionFeedback(context).sent(shade)
        assertNull(shown())
    }
}
