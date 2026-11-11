package dev.passporter.push

import android.content.Context
import android.util.Log
import dev.passporter.device.DeviceAuth
import java.io.File
import java.util.concurrent.Executors
import org.unifiedpush.android.connector.INSTANCE_DEFAULT
import org.unifiedpush.android.connector.UnifiedPush

/** The logcat tag of everything in this package. */
internal const val PUSH_TAG = "PassporterPush"

/**
 * The one worker thread [PushSync] runs on, in order, off the main thread
 * (its steps do Keystore, file and network work, and may wait on a slow
 * bridge). Incoming messages do not queue here: they are shown at once
 * ([PassporterPushService]).
 */
object PushWork {
    /** The record's file in `noBackupFilesDir`. */
    private const val RECORD_FILE = "passporter-push.json"

    /** The UnifiedPush instance: one registration per app. */
    const val INSTANCE = INSTANCE_DEFAULT

    private val executor = Executors.newSingleThreadExecutor { task ->
        Thread(task, "passporter-push").apply { isDaemon = true }
    }

    @Volatile
    private var sync: PushSync? = null

    @Volatile
    private var store: PushStore? = null

    /** Runs [task] on the push thread. A failure is logged, never fatal. */
    fun run(task: () -> Unit) {
        executor.execute {
            try {
                task()
            } catch (e: RuntimeException) {
                Log.e(PUSH_TAG, "push work failed", e)
            }
        }
    }

    /** The app's push record. Reading it is safe from any thread. */
    fun store(context: Context): PushStore = store ?: synchronized(this) {
        store ?: PushStore(File(context.applicationContext.noBackupFilesDir, RECORD_FILE)).also { store = it }
    }

    /** The app's one [PushSync]. Any context will do; the application context is kept. */
    fun sync(context: Context): PushSync = sync ?: synchronized(this) {
        sync ?: context.applicationContext.let { app ->
            PushSync(
                store = store(app),
                bridge = DeviceAuthBridge(DeviceAuth.get(app)),
                unregister = { UnifiedPush.unregister(app, INSTANCE) },
            )
        }.also { sync = it }
    }
}
