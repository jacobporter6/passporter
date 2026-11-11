package dev.passporter.recorder

import android.app.Notification
import android.content.Intent
import android.os.Looper
import java.io.File
import java.nio.file.Files
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf

@RunWith(RobolectricTestRunner::class)
class RecorderServiceTest {
    private val dir: File = Files.createTempDirectory("service").toFile()
    private val clock = RecorderTest.FakeClock()
    private val input = RecorderTest.FakeInput()
    private val tag = checkNotNull(Tag.parse("w1:p1"))
    private lateinit var recorder: Recorder

    @Before
    fun install() {
        recorder = Recorder(
            store = TakeStore(File(dir, "recordings")),
            newInput = { input },
            focus = RecorderTest.FakeFocus(),
            clock = clock,
            scheduler = RecorderTest.FakeScheduler(clock),
            newId = RecordingId::random,
            onActive = {},
        )
        Recorder.install(recorder)
    }

    @After
    fun cleanUp() {
        Recorder.install(null)
        dir.deleteRecursively()
    }

    @Test
    fun a_running_take_shows_an_ongoing_notification_with_stop() {
        recorder.start(tag, 60_000)
        val service = Robolectric.buildService(RecorderService::class.java, Intent()).create().startCommand(0, 1).get()
        settle()
        val shadow = shadowOf(service)
        assertFalse("still recording", shadow.isStoppedBySelf)
        val notification = checkNotNull(shadow.lastForegroundNotification)
        assertEquals(RecorderService.NOTIFICATION_ID, shadow.lastForegroundNotificationId)
        assertTrue(notification.flags and Notification.FLAG_ONGOING_EVENT != 0)
        assertEquals(RecorderService.CHANNEL, notification.channelId)
        val stop = notification.actions.single()
        assertEquals("Stop", stop.title.toString())
        val sent = shadowOf(stop.actionIntent).savedIntent
        assertEquals(RecorderService.ACTION_STOP, sent.action)
        assertEquals(RecorderService::class.java.name, sent.component?.className)
        assertTrue("immutable", shadowOf(stop.actionIntent).isImmutable)
    }

    @Test
    fun stop_ends_the_take_and_the_service() {
        recorder.start(tag, 60_000)
        input.write(10)
        val controller = Robolectric.buildService(RecorderService::class.java, Intent()).create().startCommand(0, 1)
        settle()
        controller.get().onStartCommand(Intent().setAction(RecorderService.ACTION_STOP), 0, 2)
        settle()
        assertEquals(EndReason.USER, recorder.status().takes.single().endedBy)
        val shadow = shadowOf(controller.get())
        assertTrue(shadow.isStoppedBySelf)
        assertTrue("the notification is gone", shadow.isForegroundStopped)
    }

    @Test
    fun a_take_already_over_stops_the_service_at_once() {
        val service = Robolectric.buildService(RecorderService::class.java, Intent()).create().startCommand(0, 1).get()
        settle()
        val shadow = shadowOf(service)
        assertTrue(shadow.isForegroundStopped)
        assertTrue(shadow.isStoppedBySelf)
    }

    @Test
    fun a_newer_take_keeps_the_service() {
        val first = recorder.start(tag, 60_000)
        input.write(1)
        val controller = Robolectric.buildService(RecorderService::class.java, Intent()).create().startCommand(0, 1)
        settle()
        recorder.stop(first)
        recorder.start(tag, 60_000)
        controller.startCommand(0, 2)
        settle()
        val shadow = shadowOf(controller.get())
        assertFalse("the second take still records in the foreground", shadow.isStoppedBySelf)
        assertFalse(shadow.isForegroundStopped)
    }

    /** Lets the recorder's thread, then the main thread, finish what they were given. */
    private fun settle() {
        repeat(3) {
            Recorder.worker.submit {}.get()
            shadowOf(Looper.getMainLooper()).idle()
        }
    }
}
