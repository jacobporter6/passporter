package dev.passporter.recorder

import java.io.File
import java.nio.file.Files
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Test

class AdtsTest {
    private val dir: File = Files.createTempDirectory("adts").toFile()
    private val file = File(dir, "take.aac")

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    @Test
    fun whole_frames_give_the_length_and_the_duration() {
        file.writeBytes(Frames.frames(250))
        // 250 frames × 1024 samples at 16 kHz.
        assertEquals(AdtsScan(25_000, 250, 16_000), Adts.scan(file))
    }

    @Test
    fun a_torn_last_frame_is_left_out() {
        file.writeBytes(Frames.frames(10) + Frames.frame().copyOf(60))
        assertEquals(AdtsScan(1_000, 10, 640), Adts.scan(file))
        file.writeBytes(Frames.frames(10) + Frames.frame().copyOf(5))
        assertEquals("a torn header", AdtsScan(1_000, 10, 640), Adts.scan(file))
    }

    @Test
    fun the_scan_stops_at_anything_that_is_not_a_frame() {
        file.writeBytes(Frames.frames(3) + ByteArray(200) + Frames.frames(3))
        assertEquals(AdtsScan(300, 3, 192), Adts.scan(file))
        val otherRate = Frames.frame(rateIndex = 3)
        file.writeBytes(Frames.frames(3) + otherRate + Frames.frames(3))
        assertEquals("a frame at another rate", 300L, Adts.scan(file).validBytes)
        val badRate = Frames.frame(rateIndex = 13)
        file.writeBytes(badRate)
        assertEquals(AdtsScan(0, 0, 0), Adts.scan(file))
    }

    @Test
    fun several_blocks_in_a_frame_all_count() {
        file.writeBytes(Frames.frame(blocks = 4))
        assertEquals(256L, Adts.scan(file).durationMs)
    }

    @Test
    fun an_empty_file_has_no_frames() {
        file.writeBytes(ByteArray(0))
        assertEquals(AdtsScan(0, 0, 0), Adts.scan(file))
    }
}
