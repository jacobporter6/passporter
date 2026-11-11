package dev.passporter.notify

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import java.io.File

/**
 * Asks for `POST_NOTIFICATIONS` (Android 13 and later) once the app is
 * connected to a bridge: notifications are useless before that, and a
 * prompt over the Connect page would come before the person knows what
 * the app is for.
 *
 * "Connected" is the shell's saved origin (`bridge-origin` in the app's
 * data directory, `apps/android/src/store.rs`). The prompt comes on the
 * first launch after connecting; Android itself stops asking after the
 * person declines twice.
 */
object NotificationPermission {
    /** The shell's saved-origin file, relative to the app's data directory. */
    private const val BRIDGE_ORIGIN_FILE = "bridge-origin"

    /**
     * Asks, if this Android needs it, the app is connected and the
     * permission is not granted yet. Call from [ComponentActivity.onCreate]:
     * the request must be registered before the activity starts.
     */
    fun requestIfConnected(activity: ComponentActivity) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val permission = Manifest.permission.POST_NOTIFICATIONS
        val granted = ContextCompat.checkSelfPermission(activity, permission) ==
            PackageManager.PERMISSION_GRANTED
        val connected = File(activity.dataDir, BRIDGE_ORIGIN_FILE).isFile
        if (granted || !connected) return
        // The answer needs no handling: notices are posted only when granted.
        activity.registerForActivityResult(ActivityResultContracts.RequestPermission()) {}
            .launch(permission)
    }
}
