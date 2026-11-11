package dev.passporter.recorder

import android.Manifest
import android.app.Activity
import android.util.Log
import app.tauri.PermissionState
import app.tauri.annotation.Command
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.Base64
import org.json.JSONObject

/**
 * The recorder, as the PWA sees it (`plugin:recorder|<command>`, granted
 * to the paired bridge's origin only by `permissions/default.toml`):
 *
 * - `start {tag, maxMs}` → `{id}`. Asks for the microphone first, if it
 *   was never granted.
 * - `stop {id}` → the [Take] (also when it had already ended by itself).
 * - `cancel {id}` → `null`: ends it without keeping it.
 * - `status {}` → [Snapshot]: the running take and the finished ones,
 *   including takes recovered from a process that died.
 * - `read {id, offset, length}` → `{data, total}`: a finished take's audio,
 *   base64, `length` at most [Requests.MAX_READ] bytes.
 * - `release {id}` → `null`: deletes a finished take the PWA has stored.
 *
 * Failures reject with `{message, code}` ([RecorderException.code]).
 * Tauri's own `checkPermissions` and `requestPermissions` commands have no
 * ACL permission, so no page reaches them.
 */
@TauriPlugin(permissions = [Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = RecorderPlugin.MIC)])
class RecorderPlugin(private val activity: Activity) : Plugin(activity) {
    private val recorder: Recorder by lazy { Recorder.get(activity) }

    @Command
    fun start(invoke: Invoke) = startOrAsk(invoke, asked = false)

    /** Where the microphone prompt answers [start]. */
    @PermissionCallback
    fun micAnswered(invoke: Invoke) = startOrAsk(invoke, asked = true)

    private fun startOrAsk(invoke: Invoke, asked: Boolean) {
        val request = try {
            Requests.start(invoke.getArgs())
        } catch (e: RecorderException) {
            invoke.reject(e.message, e.code)
            return
        }
        when (MicAccess.next(getPermissionState(MIC), asked)) {
            MicAccess.RECORD -> work(invoke) { JSONObject().put("id", recorder.start(request.tag, request.maxMs).value) }
            MicAccess.ASK -> requestPermissionForAlias(MIC, invoke, "micAnswered")
            MicAccess.REFUSE -> RecorderException.MicDenied().let { invoke.reject(it.message, it.code) }
        }
    }

    @Command
    fun stop(invoke: Invoke) = work(invoke) { args -> recorder.stop(Requests.id(args)).toJson() }

    @Command
    fun cancel(invoke: Invoke) = work(invoke) { args ->
        recorder.cancel(Requests.id(args))
        null
    }

    @Command
    fun status(invoke: Invoke) = work(invoke) { args ->
        Requests.only(args, emptySet())
        recorder.status().toJson()
    }

    @Command
    fun read(invoke: Invoke) = work(invoke) { args ->
        val request = Requests.read(args)
        val chunk = recorder.read(request.id, request.offset, request.length)
        JSONObject().put("data", Base64.getEncoder().encodeToString(chunk.bytes)).put("total", chunk.total)
    }

    @Command
    fun release(invoke: Invoke) = work(invoke) { args ->
        recorder.release(Requests.id(args))
        null
    }

    /**
     * Runs [body] on the recorder's thread with the command's arguments and
     * answers with its result or its failure (`{message, code}`).
     */
    private fun work(invoke: Invoke, body: (JSONObject) -> JSONObject?) {
        Recorder.worker.execute {
            try {
                val result = body(invoke.getArgs())
                if (result == null) invoke.resolve() else invoke.resolve(JSObject(result.toString()))
            } catch (e: RecorderException) {
                invoke.reject(e.message, e.code)
            } catch (e: Exception) {
                Log.e(TAG, "recorder command ${invoke.command} failed", e)
                invoke.reject("the recorder failed", "internal")
            }
        }
    }

    companion object {
        private const val TAG = "PassporterRecorder"

        /** The permission alias of the microphone. */
        const val MIC = "microphone"
    }
}

/** What `start` does about the microphone permission. */
internal enum class MicAccess {
    /** Granted: record. */
    RECORD,

    /** Not granted yet: ask (Android shows its prompt, or answers at once if it was denied for good). */
    ASK,

    /** Asked, and still not granted. */
    REFUSE,
    ;

    companion object {
        /** The step for permission [state], once [asked] or not yet. */
        fun next(state: PermissionState?, asked: Boolean): MicAccess = when {
            state == PermissionState.GRANTED -> RECORD
            asked -> REFUSE
            else -> ASK
        }
    }
}

/**
 * The commands' arguments, read strictly: Tauri passes the page's
 * arguments object as is, so an unknown key or a wrong type came from the
 * page and is refused ([RecorderException.InvalidInput]).
 */
internal object Requests {
    /** Longest take a page may ask for: the bridge refuses longer clips anyway (5 minutes by default). */
    const val MAX_TAKE_MS = 30L * 60 * 1000

    /** Shortest cap a page may ask for. */
    const val MIN_TAKE_MS = 1_000L

    /** Most bytes one `read` answers (before base64). */
    const val MAX_READ = 512 * 1024

    class Start(val tag: Tag, val maxMs: Long)

    class Read(val id: RecordingId, val offset: Long, val length: Int)

    fun start(args: JSONObject): Start {
        only(args, setOf("tag", "maxMs"))
        val tag = (args.opt("tag") as? String)?.let(Tag::parse) ?: throw RecorderException.InvalidInput("tag")
        val maxMs = whole(args, "maxMs")?.takeIf { it in MIN_TAKE_MS..MAX_TAKE_MS }
            ?: throw RecorderException.InvalidInput("maxMs")
        return Start(tag, maxMs)
    }

    /** `{id}`. */
    fun id(args: JSONObject): RecordingId {
        only(args, setOf("id"))
        return recordingId(args)
    }

    fun read(args: JSONObject): Read {
        only(args, setOf("id", "offset", "length"))
        val offset = whole(args, "offset")?.takeIf { it >= 0 } ?: throw RecorderException.InvalidInput("offset")
        val length = whole(args, "length")?.takeIf { it in 1..MAX_READ.toLong() }
            ?: throw RecorderException.InvalidInput("length")
        return Read(recordingId(args), offset, length.toInt())
    }

    /** Refuses any key of [args] outside [allowed]. */
    fun only(args: JSONObject, allowed: Set<String>) {
        val unknown = args.keys().asSequence().firstOrNull { it !in allowed }
        if (unknown != null) throw RecorderException.InvalidInput("argument $unknown")
    }

    private fun recordingId(args: JSONObject): RecordingId =
        (args.opt("id") as? String)?.let(RecordingId::parse) ?: throw RecorderException.InvalidInput("id")

    /** [key] as a whole number, if it is one (JavaScript numbers may arrive as `295000.0`). */
    private fun whole(args: JSONObject, key: String): Long? = when (val value = args.opt(key)) {
        is Int -> value.toLong()
        is Long -> value
        is Number -> value.toDouble().takeIf { it % 1.0 == 0.0 && kotlin.math.abs(it) <= MAX_SAFE_INTEGER }?.toLong()
        else -> null
    }

    /** The largest integer a JavaScript number holds exactly. */
    private const val MAX_SAFE_INTEGER = 9_007_199_254_740_991.0
}
