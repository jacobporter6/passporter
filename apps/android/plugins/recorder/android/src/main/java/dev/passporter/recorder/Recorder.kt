package dev.passporter.recorder

import android.content.Context
import android.os.SystemClock
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit

/** Runs work on the recorder's thread, now or later. */
internal fun interface Scheduler {
    /** Runs [task] after [delayMs]; the answer cancels it if it has not run yet. */
    fun schedule(delayMs: Long, task: () -> Unit): Cancellable
}

/** A scheduled task. */
internal fun interface Cancellable {
    fun cancel()
}

/** The two clocks a take needs. */
internal interface Clock {
    /** Monotonic, counting deep sleep: how long a take has run. */
    fun elapsedMs(): Long

    /** Wall clock, ms since the epoch: when a take ended. */
    fun wallMs(): Long
}

/**
 * The dictation recorder (design §6.3): one take at a time, recorded by
 * an [AudioInput] into a [TakeStore], kept until the PWA has read and
 * released it.
 *
 * A take ends when the PWA stops or cancels it, from the notification's
 * Stop ([stopActive]), at the length cap the PWA asked for, when a call or
 * another app takes the audio focus, or when the microphone fails. Every
 * way but a cancel keeps what was recorded as a finished [Take]. A take
 * whose process died is finished from its file when the recorder is next
 * made ([TakeStore.open]). While a take runs, [onActive] has started the
 * foreground service ([RecorderService]), which asks [watchIdle] when to
 * go.
 *
 * Calls are serialized; in the app they all run on [worker], with the
 * cap, focus and error callbacks scheduled there too.
 */
class Recorder internal constructor(
    private val store: TakeStore,
    private val newInput: () -> AudioInput,
    private val focus: AudioFocus,
    private val clock: Clock,
    private val scheduler: Scheduler,
    private val newId: () -> RecordingId,
    /** Starts the foreground service for a take that just started; may throw. */
    private val onActive: () -> Unit,
) {
    private class Running(val id: RecordingId, val tag: Tag, val startedAtMs: Long, val input: AudioInput, val cap: Cancellable)

    private val lock = Any()
    private var running: Running? = null
    private val takes: MutableList<Take> = store.open(clock.wallMs(), RETENTION_MS).toMutableList()
    private val idleWatchers = mutableListOf<() -> Unit>()

    /**
     * Starts a take for [tag] that ends by itself after [maxMs].
     *
     * @throws RecorderException [RecorderException.Busy] while another take
     *   runs, [RecorderException.MicBusy] when the audio focus is refused or
     *   the microphone does not open, [RecorderException.Storage], or
     *   [RecorderException.Failed] when the foreground service cannot start.
     */
    fun start(tag: Tag, maxMs: Long): RecordingId {
        val id = synchronized(lock) {
            if (running != null) throw RecorderException.Busy()
            val id = newId()
            val file = store.begin(id, tag, clock.wallMs())
            if (!focus.acquire { later { end(id, EndReason.FOCUS) } }) {
                store.delete(id)
                throw RecorderException.MicBusy()
            }
            val input = newInput()
            try {
                input.start(file) { later { end(id, EndReason.ERROR) } }
            } catch (e: RecorderException) {
                focus.release()
                store.delete(id)
                throw e
            }
            val cap = scheduler.schedule(maxMs) { end(id, EndReason.CAP) }
            running = Running(id, tag, clock.elapsedMs(), input, cap)
            id
        }
        try {
            onActive()
        } catch (e: Exception) {
            // Without the service the take would go silent in the background.
            cancel(id)
            throw RecorderException.Failed("the recording service did not start", e)
        }
        return id
    }

    /**
     * Stops take [id] and answers it. A take that already ended by itself
     * (the cap, a call) is answered as it ended, so a stop is never lost to
     * a race.
     *
     * @throws RecorderException [RecorderException.Empty] when it held no
     *   whole frame (it is gone), [RecorderException.NotFound] for an
     *   unknown or released id, [RecorderException.Storage].
     */
    fun stop(id: RecordingId): Take {
        if (synchronized(lock) { running?.id == id }) {
            return end(id, EndReason.USER) ?: throw RecorderException.Empty()
        }
        return synchronized(lock) { takes.firstOrNull { it.id == id } } ?: throw RecorderException.NotFound()
    }

    /** Stops the running take, if any, for [reason] (the notification's Stop). */
    fun stopActive(reason: EndReason) {
        val id = synchronized(lock) { running?.id } ?: return
        end(id, reason)
    }

    /** Ends take [id] without keeping it, running or finished. Unknown ids are ignored. */
    fun cancel(id: RecordingId) {
        val watchers = synchronized(lock) {
            val current = running
            if (current?.id == id) {
                running = null
                current.cap.cancel()
                current.input.stop()
                focus.release()
            }
            takes.removeAll { it.id == id }
            store.delete(id)
            if (current?.id == id) drainWatchers() else emptyList()
        }
        watchers.forEach { it() }
    }

    /**
     * Ends the running take, if any, and deletes every take: they were
     * recorded for the bridge the app was paired with, and the PWA of
     * another must never adopt them.
     */
    fun discardAll() {
        val ids = synchronized(lock) { listOfNotNull(running?.id) + takes.map { it.id } }
        ids.forEach(::cancel)
    }

    /** Deletes finished take [id] once the PWA has stored it. A running take or an unknown id is left alone. */
    fun release(id: RecordingId) = synchronized(lock) {
        if (takes.removeAll { it.id == id }) store.delete(id)
    }

    /** The running take and the finished ones, oldest first. */
    fun status(): Snapshot = synchronized(lock) {
        val active = running?.let { Active(it.id, it.tag, clock.elapsedMs() - it.startedAtMs, it.input.peak()) }
        Snapshot(active, takes.toList())
    }

    /** Part of a take's audio, and the take's whole size. */
    class Chunk(val bytes: ByteArray, val total: Long)

    /**
     * Up to [length] bytes of finished take [id]'s audio from [offset].
     *
     * @throws RecorderException.NotFound for a running, unknown or released take.
     */
    fun read(id: RecordingId, offset: Long, length: Int): Chunk = synchronized(lock) {
        val take = takes.firstOrNull { it.id == id } ?: throw RecorderException.NotFound()
        Chunk(store.read(id, offset, length), take.bytes)
    }

    /** Whether a take is running. */
    fun isActive(): Boolean = synchronized(lock) { running != null }

    /**
     * Runs [callback] once, when the running take ends. False, without
     * keeping [callback], when no take runs.
     */
    fun watchIdle(callback: () -> Unit): Boolean = synchronized(lock) {
        if (running == null) return false
        idleWatchers += callback
        true
    }

    /** Ends take [id], if it is the running one, keeping what it recorded. */
    private fun end(id: RecordingId, reason: EndReason): Take? {
        var watchers = emptyList<() -> Unit>()
        try {
            return synchronized(lock) {
                val current = running?.takeIf { it.id == id } ?: return null
                running = null
                watchers = drainWatchers()
                current.cap.cancel()
                current.input.stop()
                focus.release()
                store.finish(id, reason, clock.wallMs())?.also { takes += it }
            }
        } finally {
            watchers.forEach { it() }
        }
    }

    /** Runs [task] on the recorder's thread, off whichever thread a callback came in on. */
    private fun later(task: () -> Unit) {
        scheduler.schedule(0, task)
    }

    /** Takes the idle watchers. Holds [lock]. */
    private fun drainWatchers(): List<() -> Unit> = idleWatchers.toList().also { idleWatchers.clear() }

    companion object {
        /**
         * How long a finished take nobody released stays: the bridge's
         * retention, and the PWA's for its own clips (seven days).
         */
        const val RETENTION_MS = 7L * 24 * 60 * 60 * 1000

        /** Where the takes live, in `noBackupFilesDir`. */
        private const val DIR = "recordings"

        /**
         * The recorder's thread: every command, and the cap, focus and
         * error callbacks. One for the process, so a recreated activity
         * leaks none.
         */
        internal val worker: ScheduledExecutorService = Executors.newSingleThreadScheduledExecutor()

        @Volatile
        private var instance: Recorder? = null

        /** The app's one recorder (made on first use: call it from [worker]). */
        fun get(context: Context): Recorder = instance ?: synchronized(this) {
            instance ?: context.applicationContext.let { app ->
                Recorder(
                    store = TakeStore(File(app.noBackupFilesDir, DIR)),
                    newInput = { MediaRecorderInput(app) },
                    focus = SystemAudioFocus(app),
                    clock = SystemClocks,
                    scheduler = { delayMs, task ->
                        val future = worker.schedule(Runnable { task() }, delayMs, TimeUnit.MILLISECONDS)
                        Cancellable { future.cancel(false) }
                    },
                    newId = RecordingId::random,
                    onActive = { RecorderService.start(app) },
                )
            }.also { instance = it }
        }

        /**
         * [discardAll] on the app's recorder, on [worker], waiting at most
         * [DISCARD_WAIT_MS] for it. Blocking: never on the recorder's thread.
         */
        fun discardAll(context: Context) {
            val app = context.applicationContext
            worker.submit { get(app).discardAll() }.get(DISCARD_WAIT_MS, TimeUnit.MILLISECONDS)
        }

        private const val DISCARD_WAIT_MS = 5_000L

        /** Replaces the app's recorder, for tests. */
        internal fun install(recorder: Recorder?) {
            instance = recorder
        }
    }
}

/** The phone's clocks. */
private object SystemClocks : Clock {
    override fun elapsedMs() = SystemClock.elapsedRealtime()

    override fun wallMs() = System.currentTimeMillis()
}
