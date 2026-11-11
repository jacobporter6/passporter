package dev.passporter.recorder

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.os.Handler
import android.os.Looper

/** Audio focus for a take: the system's in the app, a fake in tests. */
internal interface AudioFocus {
    /**
     * Asks for focus for the length of a take; [onLost] runs (on any
     * thread) when a call or another app takes it for more than a moment.
     *
     * @return false when it is refused (a call is under way).
     */
    fun acquire(onLost: () -> Unit): Boolean

    /** Gives focus back. Harmless when none is held. */
    fun release()
}

/**
 * [AudioFocus] through `AudioManager`, asked for as exclusive and
 * transient: other audio pauses (no music under a dictation) and comes
 * back after. A loss that lets the take duck (a notification sound, a
 * navigation prompt) is ignored; any other loss (a ringing or ongoing
 * call, another app recording or playing) ends the take, keeping what was
 * said, rather than recording on silenced or resuming unnoticed later.
 */
internal class SystemAudioFocus(context: Context) : AudioFocus {
    private val manager = context.getSystemService(AudioManager::class.java)
    private var held: AudioFocusRequest? = null

    override fun acquire(onLost: () -> Unit): Boolean {
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE)
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build(),
            )
            .setOnAudioFocusChangeListener({ change -> if (endsTake(change)) onLost() }, Handler(Looper.getMainLooper()))
            .build()
        val granted = manager.requestAudioFocus(request) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED
        if (granted) held = request
        return granted
    }

    override fun release() {
        held?.let(manager::abandonAudioFocusRequest)
        held = null
    }

    companion object {
        /** Whether a focus [change] ends the take. */
        fun endsTake(change: Int): Boolean =
            change == AudioManager.AUDIOFOCUS_LOSS || change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT
    }
}
