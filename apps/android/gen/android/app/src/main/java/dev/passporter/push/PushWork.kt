package dev.passporter.push

import android.content.Context
import android.util.Log
import dev.passporter.device.DeviceAuth
import java.io.File
import java.util.concurrent.Executors
import org.unifiedpush.android.connector.INSTANCE_DEFAULT
import org.unifiedpush.android.connector.UnifiedPush

/**
 * The one worker thread push runs on: every [PushSync] step and every
 * incoming message, in order, off the main thread (they do Keystore,
 * file and network work).
 */
object PushWork {
    private const val TAG = "PassporterPush"

    /** The record's file in `noBackupFilesDir`. */
    private const val RECORD_FILE = "passporter-push.json"

    /** The UnifiedPush instance: one registration per app. */
    const val INSTANCE = INSTANCE_DEFAULT

    private val executor = Executors.newSingleThreadExecutor { task ->
        Thread(task, "passporter-push").apply { isDaemon = true }
    }

    @Volatile
    private var sync: PushSync? = null

    /** Runs [task] on the push thread. A failure is logged, never fatal. */
    fun run(task: () -> Unit) {
        executor.execute {
            try {
                task()
            } catch (e: RuntimeException) {
                Log.e(TAG, "push work failed", e)
            }
        }
    }

    /** The app's one [PushSync]. Any context will do; the application context is kept. */
    fun sync(context: Context): PushSync = sync ?: synchronized(this) {
        sync ?: context.applicationContext.let { app ->
            PushSync(
                store = PushStore(File(app.noBackupFilesDir, RECORD_FILE)),
                bridge = DeviceAuthBridge(DeviceAuth.get(app)),
                unregister = { UnifiedPush.unregister(app, INSTANCE) },
            )
        }.also { sync = it }
    }
}
