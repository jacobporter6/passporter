package dev.passporter.recorder

import java.io.File
import java.nio.file.Files
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class RecorderTest {
    private val dir: File = Files.createTempDirectory("recorder").toFile()
    private val clock = FakeClock()
    private val scheduler = FakeScheduler(clock)
    private val focus = FakeFocus()
    private val inputs = mutableListOf<FakeInput>()
    private var serviceStarts = 0
    private var serviceFails = false
    private val tag = checkNotNull(Tag.parse("w1:p1"))
    private val recorder = recorder()

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    private fun recorder() = Recorder(
        store = TakeStore(File(dir, "recordings")),
        newInput = { FakeInput().also { inputs += it } },
        focus = focus,
        clock = clock,
        scheduler = scheduler,
        newId = RecordingId::random,
        onActive = {
            if (serviceFails) throw IllegalStateException("not allowed")
            serviceStarts++
        },
    )

    private val input get() = inputs.last()

    @Test
    fun a_take_records_until_stopped() {
        val id = recorder.start(tag, 60_000)
        assertEquals(1, serviceStarts)
        assertTrue(focus.held)
        clock.advance(3_200)
        input.write(50)
        input.peak = 0.25f
        assertEquals(Snapshot(Active(id, tag, 3_200, 0.25f), emptyList()), recorder.status())

        val take = recorder.stop(id)
        assertEquals(Take(id, tag, 3_200, 5_000, EndReason.USER, clock.wall), take)
        assertTrue(input.stopped)
        assertFalse(focus.held)
        assertEquals(Snapshot(null, listOf(take)), recorder.status())
        assertEquals("the cap is cancelled", 0, scheduler.pending())
    }

    @Test
    fun one_take_at_a_time() {
        recorder.start(tag, 60_000)
        assertThrows(RecorderException.Busy::class.java) { recorder.start(tag, 60_000) }
        assertEquals(1, inputs.size)
    }

    @Test
    fun the_cap_ends_a_take_and_a_late_stop_still_gets_it() {
        val id = recorder.start(tag, 10_000)
        input.write(20)
        clock.advance(10_000)
        scheduler.runDue()
        val take = recorder.status().takes.single()
        assertEquals(EndReason.CAP, take.endedBy)
        assertNull(recorder.status().active)
        assertEquals("stopping it now answers how it ended", take, recorder.stop(id))
    }

    @Test
    fun losing_the_audio_focus_ends_the_take_keeping_what_was_said() {
        recorder.start(tag, 60_000)
        input.write(20)
        focus.lose()
        scheduler.runDue()
        assertEquals(EndReason.FOCUS, recorder.status().takes.single().endedBy)
        assertTrue(input.stopped)
    }

    @Test
    fun a_microphone_error_ends_the_take_keeping_what_was_said() {
        recorder.start(tag, 60_000)
        input.write(20)
        input.fail()
        scheduler.runDue()
        assertEquals(EndReason.ERROR, recorder.status().takes.single().endedBy)
    }

    @Test
    fun a_refused_focus_or_a_busy_microphone_starts_nothing() {
        focus.grants = false
        assertThrows(RecorderException.MicBusy::class.java) { recorder.start(tag, 60_000) }
        assertTrue(inputs.isEmpty())
        focus.grants = true
        FakeInput.refuseNext = true
        assertThrows(RecorderException.MicBusy::class.java) { recorder.start(tag, 60_000) }
        assertFalse("focus is given back", focus.held)
        assertEquals(Snapshot(null, emptyList()), recorder.status())
        assertTrue("no files left", File(dir, "recordings").list().orEmpty().isEmpty())
        assertEquals(0, serviceStarts)
    }

    @Test
    fun a_service_that_cannot_start_cancels_the_take() {
        serviceFails = true
        assertThrows(RecorderException.Failed::class.java) { recorder.start(tag, 60_000) }
        assertTrue(input.stopped)
        assertFalse(focus.held)
        assertEquals(Snapshot(null, emptyList()), recorder.status())
        assertTrue(File(dir, "recordings").list().orEmpty().isEmpty())
    }

    @Test
    fun a_take_with_nothing_in_it_is_empty_and_gone() {
        val id = recorder.start(tag, 60_000)
        assertThrows(RecorderException.Empty::class.java) { recorder.stop(id) }
        assertEquals(Snapshot(null, emptyList()), recorder.status())
        assertThrows(RecorderException.NotFound::class.java) { recorder.stop(id) }
    }

    @Test
    fun cancel_keeps_nothing() {
        val id = recorder.start(tag, 60_000)
        input.write(20)
        recorder.cancel(id)
        assertEquals(Snapshot(null, emptyList()), recorder.status())
        assertTrue(File(dir, "recordings").list().orEmpty().isEmpty())
        recorder.cancel(id) // unknown now: ignored
        val finished = recorder.start(tag, 60_000).also { input.write(5) }
        recorder.stop(finished)
        recorder.cancel(finished)
        assertTrue("a finished take is cancelled too", recorder.status().takes.isEmpty())
    }

    @Test
    fun a_finished_take_is_read_in_chunks_then_released() {
        val id = recorder.start(tag, 60_000)
        val audio = input.write(30)
        assertThrows("not while it records", RecorderException.NotFound::class.java) { recorder.read(id, 0, 10) }
        recorder.stop(id)
        val first = recorder.read(id, 0, 2_000)
        val rest = recorder.read(id, 2_000, 2_000)
        assertEquals(3_000L, first.total)
        assertArrayEquals(audio, first.bytes + rest.bytes)
        recorder.release(id)
        assertTrue(recorder.status().takes.isEmpty())
        assertThrows(RecorderException.NotFound::class.java) { recorder.read(id, 0, 10) }
        recorder.release(id) // again: harmless
    }

    @Test
    fun release_leaves_a_running_take_alone() {
        val id = recorder.start(tag, 60_000)
        recorder.release(id)
        assertEquals(id, recorder.status().active?.id)
    }

    @Test
    fun a_take_cut_short_by_process_death_is_there_after_a_restart() {
        val id = recorder.start(tag, 60_000)
        input.write(40)
        // The process dies: nothing stops the input, and a new process
        // makes a new recorder over the same files.
        val after = recorder()
        val take = after.status().takes.single()
        assertEquals(id, take.id)
        assertEquals(EndReason.INTERRUPTED, take.endedBy)
        assertEquals(2_560L, take.durationMs)
        assertNull(after.status().active)
        assertEquals(4_000, after.read(id, 0, 10_000).bytes.size)
    }

    @Test
    fun the_service_is_told_when_the_take_ends() {
        var idle = 0
        assertFalse("nothing to watch", recorder.watchIdle { idle++ })
        val id = recorder.start(tag, 60_000)
        input.write(1)
        assertTrue(recorder.watchIdle { idle++ })
        recorder.stop(id)
        assertEquals(1, idle)
        val next = recorder.start(tag, 60_000)
        assertTrue(recorder.watchIdle { idle++ })
        recorder.cancel(next)
        assertEquals("once per watch", 2, idle)
    }

    @Test
    fun the_notifications_stop_ends_the_running_take() {
        recorder.stopActive(EndReason.USER) // nothing running: nothing happens
        recorder.start(tag, 60_000)
        input.write(3)
        recorder.stopActive(EndReason.USER)
        assertEquals(EndReason.USER, recorder.status().takes.single().endedBy)
    }

    internal class FakeClock : Clock {
        var elapsed = 5_000L
        var wall = 1_767_225_600_000L

        fun advance(ms: Long) {
            elapsed += ms
            wall += ms
        }

        override fun elapsedMs() = elapsed

        override fun wallMs() = wall
    }

    /** Runs tasks only when told, once their time has come. */
    internal class FakeScheduler(private val clock: FakeClock) : Scheduler {
        private class Task(val at: Long, val run: () -> Unit) {
            var cancelled = false
        }

        private val tasks = mutableListOf<Task>()

        override fun schedule(delayMs: Long, task: () -> Unit): Cancellable {
            val scheduled = Task(clock.elapsed + delayMs, task)
            tasks += scheduled
            return Cancellable { scheduled.cancelled = true }
        }

        fun pending(): Int = tasks.count { !it.cancelled }

        fun runDue() {
            while (true) {
                val due = tasks.firstOrNull { !it.cancelled && it.at <= clock.elapsed } ?: return
                tasks.remove(due)
                due.run()
            }
        }
    }

    internal class FakeFocus : AudioFocus {
        var grants = true
        var held = false
        private var onLost: (() -> Unit)? = null

        override fun acquire(onLost: () -> Unit): Boolean {
            if (!grants) return false
            held = true
            this.onLost = onLost
            return true
        }

        override fun release() {
            held = false
        }

        fun lose() = checkNotNull(onLost)()
    }

    /** Writes frames into the take's file when told, like the encoder would. */
    internal class FakeInput : AudioInput {
        var peak = 0f
        var stopped = false
        private var file: File? = null
        private var onError: (() -> Unit)? = null

        override fun start(file: File, onError: () -> Unit) {
            if (refuseNext) {
                refuseNext = false
                throw RecorderException.MicBusy()
            }
            this.file = file
            this.onError = onError
        }

        override fun peak() = peak

        override fun stop() {
            stopped = true
        }

        /** Appends [frames] 100-byte frames (64 ms each) and answers them. */
        fun write(frames: Int): ByteArray = Frames.frames(frames).also { checkNotNull(file).appendBytes(it) }

        fun fail() = checkNotNull(onError)()

        companion object {
            var refuseNext = false
        }
    }
}
