package dev.passporter.app

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import androidx.core.app.NotificationCompat
import androidx.work.Configuration
import androidx.work.WorkInfo
import androidx.work.WorkManager
import androidx.work.testing.SynchronousExecutor
import androidx.work.testing.WorkManagerTestInitHelper
import dev.passporter.car.CarOutbox
import dev.passporter.car.carJob
import dev.passporter.device.DeviceAuth
import dev.passporter.notify.DEVICE
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.actions.NoticeAction
import dev.passporter.notify.actions.Outbox
import dev.passporter.notify.actions.PreferenceFrozenBodies
import dev.passporter.notify.actions.job
import dev.passporter.notify.notice
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

/** What a pairing change drops: nothing of the old pairing may reach the next one. */
@RunWith(RobolectricTestRunner::class)
class PairingResetTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val manager = context.getSystemService(NotificationManager::class.java)

    /** Another notification of the app's: the dictation recorder's own, say. */
    private val otherId = 0x7EC0

    @Before
    fun setUp() {
        shadowOf(context).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        // No worker ever runs: their constraints are never met.
        WorkManagerTestInitHelper.initializeTestWorkManager(
            context,
            Configuration.Builder().setExecutor(SynchronousExecutor()).build(),
        )
        NoticeNotification.createChannels(context)
        Outbox.enqueue(context, job(NoticeAction.REPLY, "hi"))
        CarOutbox.enqueue(context, carJob())
        PreferenceFrozenBodies(context).put("key", "{}")
        val notice = notice()
        manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, NoticeNotification.build(context, notice, 1, DEVICE))
        val other = NotificationCompat.Builder(context, "dictation").setSmallIcon(android.R.drawable.ic_btn_speak_now).build()
        manager.notify(otherId, other)
    }

    private fun states(tag: String) = WorkManager.getInstance(context).getWorkInfosByTag(tag).get().map { it.state }

    private fun assertReset() {
        assertEquals(listOf(WorkInfo.State.CANCELLED), states(Outbox.TAG))
        assertEquals(listOf(WorkInfo.State.CANCELLED), states(CarOutbox.TAG))
        assertTrue(PreferenceFrozenBodies(context).keys().isEmpty())
        assertEquals("the recorder's notification stays", listOf(otherId), shadowOf(manager).activeNotifications.map { it.id })
    }

    @Test
    fun itDropsTheQueuedAnswersTheirBodiesAndTheAgentsNotifications() {
        assertEquals(listOf(WorkInfo.State.ENQUEUED), states(Outbox.TAG))
        assertEquals(2, shadowOf(manager).activeNotifications.size)
        PairingReset.run(context)
        assertReset()
        PairingReset.run(context) // idempotent
        assertReset()
    }

    /** The application's listener: forgetting the pairing (from the PWA too) resets. */
    @Test
    fun everyPairingChangeInTheProcessResets() {
        DeviceAuth.get(context).forget()
        assertReset()
    }
}
