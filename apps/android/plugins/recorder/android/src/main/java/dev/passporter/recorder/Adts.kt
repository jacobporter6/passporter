package dev.passporter.recorder

import java.io.EOFException
import java.io.File
import java.io.RandomAccessFile

/**
 * What [Adts.scan] found in a file of AAC frames: how much of it is whole
 * frames, and how long they play.
 */
data class AdtsScan(
    /** Bytes of whole, valid frames from the start; anything after is a torn or foreign tail. */
    val validBytes: Long,
    /** Whole frames. */
    val frames: Long,
    /** Their length in ms (0 for no frames). */
    val durationMs: Long,
)

/**
 * ADTS, the framing `MediaRecorder` writes for `OutputFormat.AAC_ADTS`:
 * every AAC frame carries a 7-byte header (9 with a CRC) with its own
 * length. Unlike MP4, whose index is written only when recording stops,
 * every whole frame of an ADTS file decodes on its own, so a recording cut
 * short by a killed process is still good up to its last whole frame.
 */
object Adts {
    /** Samples per AAC frame (per raw data block). */
    private const val SAMPLES_PER_BLOCK = 1024L

    /** ADTS header length without a CRC. */
    private const val HEADER = 7

    /** Sampling frequencies by the header's 4-bit index (ISO/IEC 14496-3). */
    private val RATES = intArrayOf(96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350)

    /**
     * Walks [file]'s frames from the start and stops at the first that is
     * torn (runs past the end) or not an ADTS frame. Every frame must keep
     * the first frame's sampling rate.
     */
    fun scan(file: File): AdtsScan = RandomAccessFile(file, "r").use { input ->
        val length = input.length()
        val header = ByteArray(HEADER)
        var offset = 0L
        var blocks = 0L
        var frames = 0L
        var rate = 0
        while (offset + HEADER <= length) {
            input.seek(offset)
            try {
                input.readFully(header)
            } catch (e: EOFException) {
                break
            }
            val frame = parse(header) ?: break
            if (rate != 0 && frame.rate != rate) break
            if (offset + frame.length > length) break
            rate = frame.rate
            offset += frame.length
            blocks += frame.blocks
            frames++
        }
        AdtsScan(offset, frames, if (rate == 0) 0 else blocks * SAMPLES_PER_BLOCK * 1000 / rate)
    }

    private class Frame(val length: Int, val rate: Int, val blocks: Int)

    /** One header, if it is a valid ADTS header for AAC. */
    private fun parse(h: ByteArray): Frame? {
        val b = IntArray(HEADER) { h[it].toInt() and 0xFF }
        // Syncword 0xFFF, then MPEG version (either), layer 00.
        if (b[0] != 0xFF || (b[1] and 0xF6) != 0xF0) return null
        val protectionAbsent = (b[1] and 0x01) == 1
        val rateIndex = (b[2] shr 2) and 0x0F
        if (rateIndex >= RATES.size) return null
        val length = ((b[3] and 0x03) shl 11) or (b[4] shl 3) or (b[5] shr 5)
        val headerLength = if (protectionAbsent) HEADER else HEADER + 2
        if (length <= headerLength) return null
        val blocks = (b[6] and 0x03) + 1
        return Frame(length, RATES[rateIndex], blocks)
    }
}
