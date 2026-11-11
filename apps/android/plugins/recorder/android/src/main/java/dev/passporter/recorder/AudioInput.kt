package dev.passporter.recorder

import android.content.Context
import android.media.MediaRecorder
import android.os.Build
import java.io.File

/** The microphone, encoding into a file: `MediaRecorder` on a phone, a fake in tests. */
internal interface AudioInput {
    /**
     * Starts recording into [file]; [onError] runs (on any thread) if
     * recording breaks down later.
     *
     * @throws RecorderException.MicBusy when the microphone does not open.
     */
    fun start(file: File, onError: () -> Unit)

    /** The loudest sample since the last call, in `0.0..=1.0`. */
    fun peak(): Float

    /** Stops and lets go of the microphone. Whatever was written stays in the file. Never throws. */
    fun stop()
}

/**
 * [AudioInput] over `MediaRecorder`: AAC-LC in ADTS frames ([Adts]), mono,
 * 16 kHz (what whisper resamples to anyway) at 32 kbit/s, from the source
 * tuned for speech recognition. `MediaRecorder` writes each frame as it is
 * encoded, so a killed process loses only the frames still in the encoder.
 */
internal class MediaRecorderInput(private val context: Context) : AudioInput {
    private var recorder: MediaRecorder? = null

    override fun start(file: File, onError: () -> Unit) {
        val recorder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            MediaRecorder(context)
        } else {
            @Suppress("DEPRECATION")
            MediaRecorder()
        }
        try {
            recorder.setAudioSource(MediaRecorder.AudioSource.VOICE_RECOGNITION)
            recorder.setOutputFormat(MediaRecorder.OutputFormat.AAC_ADTS)
            recorder.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            recorder.setAudioChannels(1)
            recorder.setAudioSamplingRate(SAMPLE_RATE)
            recorder.setAudioEncodingBitRate(BIT_RATE)
            recorder.setOutputFile(file.path)
            recorder.setOnErrorListener { _, _, _ -> onError() }
            recorder.prepare()
            recorder.start()
        } catch (e: Exception) {
            // IllegalStateException, IOException or RuntimeException: the
            // microphone is held elsewhere, or the encoder refused.
            recorder.release()
            throw RecorderException.MicBusy(e)
        }
        this.recorder = recorder
    }

    override fun peak(): Float = try {
        (recorder?.maxAmplitude ?: 0) / MAX_AMPLITUDE
    } catch (e: IllegalStateException) {
        0f
    }

    override fun stop() {
        val recorder = recorder ?: return
        this.recorder = null
        try {
            recorder.stop()
        } catch (e: RuntimeException) {
            // Stopped before the first frame, or after an error: the file
            // holds what it holds, and the scan decides.
        } finally {
            recorder.release()
        }
    }

    private companion object {
        const val SAMPLE_RATE = 16_000
        const val BIT_RATE = 32_000
        const val MAX_AMPLITUDE = 32767f
    }
}
