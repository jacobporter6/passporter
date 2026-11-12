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
 * data directory, `apps/android/src/store.rs`). [askIfDue] runs whenever
 * the activity resumes, and asks at most once per activity: after
 * connecting, the first return to the app (for example from the camera
 * prompt of the pairing scanner) asks. Android itself stops asking after
 * the person declines twice.
 *
 * While its prompt shows ([asking]) other prompts wait, so two never come
 * on one resume; [onAnswered] is called once the person has answered.
 *
 * Create it while [activity] is being constructed or created: the request
 * must be registered before the activity starts.
 */
class NotificationPermission(
    private val activity: ComponentActivity,
    /** Called on the main thread once the person answered the prompt, either way. */
    private val onAnswered: () -> Unit,
) {
    // Notices are posted only when granted: the answer itself needs no handling.
    private val request =
        activity.registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            asking = false
            onAnswered()
        }

    /** Whether this activity asked already. */
    private var asked = false

    /** Whether the prompt is up (asked, not answered yet). Main thread only. */
    var asking = false
        private set

    /** Asks if this Android needs it, the app is connected, and it was neither granted nor asked. */
    fun askIfDue() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU || asked) return
        val permission = Manifest.permission.POST_NOTIFICATIONS
        val granted = ContextCompat.checkSelfPermission(activity, permission) ==
            PackageManager.PERMISSION_GRANTED
        val connected = File(activity.dataDir, BRIDGE_ORIGIN_FILE).isFile
        if (granted || !connected) return
        asked = true
        asking = true
        request.launch(permission)
    }

    private companion object {
        /** The shell's saved-origin file, relative to the app's data directory. */
        const val BRIDGE_ORIGIN_FILE = "bridge-origin"
    }
}
