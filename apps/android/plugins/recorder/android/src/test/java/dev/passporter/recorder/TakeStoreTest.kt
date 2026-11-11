package dev.passporter.recorder

import java.io.File
import java.nio.file.Files
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class TakeStoreTest {
    private val dir: File = Files.createTempDirectory("takes").toFile()
    private val store = TakeStore(File(dir, "recordings"))
    private val id = RecordingId.random()
    private val tag = checkNotNull(Tag.parse("w1:p1"))

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    @Test
    fun a_finished_take_keeps_its_whole_frames() {
        val audio = store.begin(id, tag, 1_000)
        audio.writeBytes(Frames.frames(50) + Frames.frame().copyOf(30))
        val take = checkNotNull(store.finish(id, EndReason.USER, 9_000))
        assertEquals(Take(id, tag, 3_200, 5_000, EndReason.USER, 9_000), take)
        assertEquals("the torn tail is cut off", 5_000L, audio.length())
        assertEquals(listOf(take), store.open(10_000, DAY))
    }

    @Test
    fun a_take_without_a_whole_frame_is_deleted() {
        store.begin(id, tag, 1_000).writeBytes(Frames.frame().copyOf(40))
        assertNull(store.finish(id, EndReason.USER, 2_000))
        assertTrue(store.open(3_000, DAY).isEmpty())
        assertTrue(files().isEmpty())
    }

    @Test
    fun a_take_left_recording_by_a_dead_process_is_recovered() {
        val audio = store.begin(id, tag, 1_000)
        audio.writeBytes(Frames.frames(100) + Frames.frame().copyOf(50))
        audio.setLastModified(60_000)
        // A new process: nothing is recording now.
        val takes = TakeStore(File(dir, "recordings")).open(70_000, DAY)
        val take = takes.single()
        assertEquals(EndReason.INTERRUPTED, take.endedBy)
        assertEquals(6_400L, take.durationMs)
        assertEquals(10_000L, take.bytes)
        assertEquals("ended when it was last written", 60_000L, take.endedAtMs)
        assertEquals("finished on disk too", takes, store.open(80_000, DAY))
    }

    @Test
    fun old_takes_and_debris_are_swept_at_start() {
        store.begin(id, tag, 1_000).writeBytes(Frames.frames(5))
        store.finish(id, EndReason.USER, 2_000)
        val recent = RecordingId.random()
        store.begin(recent, tag, 1_000).writeBytes(Frames.frames(5))
        store.finish(recent, EndReason.CAP, 2_000 + DAY)
        val folder = File(dir, "recordings")
        File(folder, "${RecordingId.random()}.aac").writeBytes(Frames.frames(5)) // audio without a journal
        File(folder, "$recent.json.tmp").writeText("{") // a half-written journal
        val broken = RecordingId.random()
        File(folder, "$broken.json").writeText("not json")
        File(folder, "$broken.aac").writeBytes(Frames.frames(5))
        val forged = RecordingId.random()
        File(folder, "$forged.json").writeText(JSONObject().put("id", id.value).put("state", "finished").toString())

        val takes = store.open(2_001 + DAY, DAY)
        assertEquals(listOf(recent), takes.map { it.id })
        assertEquals(setOf("$recent.aac", "$recent.json"), files())
    }

    @Test
    fun a_finished_journal_must_match_its_audio() {
        store.begin(id, tag, 1_000).writeBytes(Frames.frames(5))
        store.finish(id, EndReason.USER, 2_000)
        store.audio(id).appendBytes(ByteArray(3))
        assertTrue("a changed file is not that take", store.open(3_000, DAY).isEmpty())
        assertTrue(files().isEmpty())
    }

    @Test
    fun reads_are_bounded_by_the_file() {
        val audio = Frames.frames(3)
        store.begin(id, tag, 1_000).writeBytes(audio)
        store.finish(id, EndReason.USER, 2_000)
        assertArrayEquals(audio.copyOfRange(0, 128), store.read(id, 0, 128))
        assertArrayEquals(audio.copyOfRange(256, 300), store.read(id, 256, 128))
        assertEquals(0, store.read(id, 300, 128).size)
        assertEquals(0, store.read(id, 9_999, 128).size)
        assertThrows(RecorderException.NotFound::class.java) { store.read(RecordingId.random(), 0, 1) }
    }

    @Test
    fun delete_removes_both_files() {
        store.begin(id, tag, 1_000).writeBytes(Frames.frames(3))
        store.delete(id)
        assertTrue(files().isEmpty())
        assertFalse(store.audio(id).exists())
    }

    private fun files(): Set<String> = File(dir, "recordings").list().orEmpty().toSet()

    private companion object {
        const val DAY = 86_400_000L
    }
}
