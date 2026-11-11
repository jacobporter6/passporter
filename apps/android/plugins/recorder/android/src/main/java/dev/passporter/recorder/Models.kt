package dev.passporter.recorder

import java.util.UUID
import org.json.JSONObject

/**
 * A take's id: a lowercase UUID, made here when the take starts. The PWA
 * keeps it as the clip's `audioId`, so storing a take twice (a page that
 * died between storing and releasing it) stores the same clip. Only a
 * parsed id ever names a file, so an id is never a path.
 */
@JvmInline
value class RecordingId private constructor(val value: String) {
    override fun toString() = value

    companion object {
        private val SHAPE = Regex("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")

        /** [raw] if it is a lowercase UUID (the bridge's `AudioId` rule), else null. */
        fun parse(raw: String): RecordingId? = if (SHAPE.matches(raw)) RecordingId(raw) else null

        /** A new random id. */
        fun random(): RecordingId = RecordingId(UUID.randomUUID().toString())
    }
}

/**
 * What the PWA attached to a take: the agent it is for. Opaque here; kept
 * with the take so a take recovered after the page died goes back to the
 * right agent. At most [MAX_LENGTH] characters, no control characters.
 */
@JvmInline
value class Tag private constructor(val value: String) {
    override fun toString() = value

    companion object {
        const val MAX_LENGTH = 200

        /** [raw] if it is a valid tag, else null. */
        fun parse(raw: String): Tag? = raw.takeIf { tag ->
            tag.isNotEmpty() && tag.length <= MAX_LENGTH && tag.none { it.isISOControl() }
        }?.let(::Tag)
    }
}

/** Why a take ended. [wire] is what the PWA reads (`EndedBy` in `crates/web/src/voice/native.rs`). */
enum class EndReason(val wire: String) {
    /** The PWA's stop, or the notification's Stop. */
    USER("user"),

    /** It reached the length cap the PWA asked for. */
    CAP("cap"),

    /** A call or another app took the microphone (audio focus lost). */
    FOCUS("focus"),

    /** The microphone or the encoder failed; what was written is kept. */
    ERROR("error"),

    /** The app's process died while it recorded; recovered from its file at the next start. */
    INTERRUPTED("interrupted"),
    ;

    companion object {
        /** The reason named [wire], if any. */
        fun of(wire: String): EndReason? = entries.firstOrNull { it.wire == wire }
    }
}

/**
 * A finished take, waiting for the PWA to read and release it. Its audio
 * is AAC in ADTS frames ([MIME]), every frame whole.
 */
data class Take(
    val id: RecordingId,
    val tag: Tag,
    /** Length of the audio, from its frames. At least 1. */
    val durationMs: Long,
    /** Size of the audio file. */
    val bytes: Long,
    val endedBy: EndReason,
    /** When it ended, wall clock (ms since the epoch). */
    val endedAtMs: Long,
) {
    /** As the plugin answers it: `{id, tag, durationMs, bytes, mime, endedBy, endedAt}`. */
    fun toJson(): JSONObject = JSONObject()
        .put("id", id.value)
        .put("tag", tag.value)
        .put("durationMs", durationMs)
        .put("bytes", bytes)
        .put("mime", MIME)
        .put("endedBy", endedBy.wire)
        .put("endedAt", endedAtMs)

    companion object {
        /** AAC in ADTS frames: what whisper-server's `--convert` (ffmpeg) decodes as AAC. */
        const val MIME = "audio/aac"
    }
}

/** The take being recorded, as `status` reports it. */
data class Active(
    val id: RecordingId,
    val tag: Tag,
    /** Recorded so far. */
    val elapsedMs: Long,
    /** The loudest sample since the last report, in `0.0..=1.0`. */
    val peak: Float,
) {
    /** `{id, tag, elapsedMs, peak}`. */
    fun toJson(): JSONObject = JSONObject()
        .put("id", id.value)
        .put("tag", tag.value)
        .put("elapsedMs", elapsedMs)
        .put("peak", peak.toDouble())
}

/** Everything `status` reports: the running take, if any, and the finished takes, oldest first. */
data class Snapshot(val active: Active?, val takes: List<Take>) {
    /** `{active?, takes}`. */
    fun toJson(): JSONObject = JSONObject().apply {
        active?.let { put("active", it.toJson()) }
        put("takes", org.json.JSONArray().apply { takes.forEach { put(it.toJson()) } })
    }
}

/**
 * Why a recorder call failed. [code] is stable: the PWA maps it
 * (`crates/web/src/voice/native.rs`).
 */
sealed class RecorderException(
    /** A stable, camelCase name for the failure. */
    val code: String,
    message: String,
    cause: Throwable? = null,
) : Exception(message, cause) {
    /** The person refused the microphone permission. */
    class MicDenied : RecorderException("micDenied", "the microphone permission was refused")

    /** A call or another app holds the audio, or the microphone would not open. */
    class MicBusy(cause: Throwable? = null) :
        RecorderException("micBusy", "the microphone is in use by a call or another app", cause)

    /** A take is already recording. */
    class Busy : RecorderException("busy", "already recording")

    /** No take with that id (never started, or already released). */
    class NotFound : RecorderException("notFound", "no such recording")

    /** The take ended without a single whole audio frame. */
    class Empty : RecorderException("empty", "nothing was recorded")

    /** App-private storage failed. */
    class Storage(cause: Throwable) :
        RecorderException("storage", "could not save the recording: ${cause.message ?: cause.javaClass.simpleName}", cause)

    /** Recording could not start or go on (the recording service, say). */
    class Failed(detail: String, cause: Throwable? = null) : RecorderException("failed", "recording failed: $detail", cause)

    /** The caller passed something that is not valid. */
    class InvalidInput(field: String) : RecorderException("invalidInput", "invalid $field")
}
