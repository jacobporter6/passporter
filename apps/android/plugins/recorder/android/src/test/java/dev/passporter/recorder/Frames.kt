package dev.passporter.recorder

import java.io.File

/** Synthetic ADTS frames, shaped like `MediaRecorder`'s AAC_ADTS output. */
object Frames {
    /** The 16 kHz sampling-rate index: 1024 samples last 64 ms. */
    const val RATE_16K = 8

    /** One frame of [size] bytes (7-byte header, no CRC) at [rateIndex], with [blocks] raw data blocks. */
    fun frame(size: Int = 100, rateIndex: Int = RATE_16K, blocks: Int = 1): ByteArray {
        require(size in 8..0x1FFF)
        val frame = ByteArray(size) { 0x21 }
        frame[0] = 0xFF.toByte()
        frame[1] = 0xF1.toByte() // MPEG-4, layer 0, no CRC
        frame[2] = ((1 shl 6) or (rateIndex shl 2)).toByte() // AAC-LC, rate, mono (high bit 0)
        frame[3] = ((1 shl 6) or ((size shr 11) and 0x03)).toByte() // mono (low bits), length high
        frame[4] = ((size shr 3) and 0xFF).toByte()
        frame[5] = (((size and 0x07) shl 5) or 0x1F).toByte()
        frame[6] = (0xFC or ((blocks - 1) and 0x03)).toByte()
        return frame
    }

    /** [count] frames of [size] bytes back to back. */
    fun frames(count: Int, size: Int = 100): ByteArray =
        (0 until count).fold(ByteArray(0)) { all, _ -> all + frame(size) }

    /** Appends [count] frames to [file]. */
    fun append(file: File, count: Int, size: Int = 100) = file.appendBytes(frames(count, size))
}
