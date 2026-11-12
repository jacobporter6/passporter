package dev.passporter.notify

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import dev.passporter.device.DeviceAuth
import dev.passporter.device.PairingListener
import java.io.File

/**
 * Asks for `POST_NOTIFICATIONS` (Android 13 and later) once the app is
 * connected to a bridge: notifications are useless before that, and a
 * prompt over the Connect page would come before the person knows what
 * the app is for.
 *
 * "Connected" is the shell's saved origin (`bridge-origin` in the app's
 * data directory, `apps/android/src/store.rs`). [askIfDue] runs whenever
 * the activity resumes and whenever the app pairs ([pairingListener]),
 * and asks at most once per activity: after connecting, the first return
 * to the app (for example from the camera prompt of the pairing scanner)
 * asks, and so does pairing by typed code, which involves no return at
 * all (without it, every notice until the next resume would be dropped).
 * Android itself stops asking after the person declines twice.
 *
 * While its prompt shows ([asking]) other prompts wait, so two never come
 * on one resume or pairing; [onAnswered] is called once the person has
 * answered. A pairing posts its ask to the main thread straight away,
 * ahead of push's distributor picker, which comes back from the push
 * thread only after its own post (`dev.passporter.push.PushRegistration`).
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

    /**
     * Asks once the app has paired (not when it forgets), on the main
     * thread, if the activity is in front; otherwise its next resume asks.
     */
    internal val pairingListener = PairingListener { status ->
        if (status.paired) {
            activity.runOnUiThread {
                if (activity.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) askIfDue()
            }
        }
    }

    init {
        activity.lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onCreate(owner: LifecycleOwner) =
                DeviceAuth.get(activity.applicationContext).addPairingListener(pairingListener)

            override fun onDestroy(owner: LifecycleOwner) =
                DeviceAuth.get(activity.applicationContext).removePairingListener(pairingListener)
        })
    }

    /** Whether this activity asked already. */
    private var asked = false

    /** Whether the prompt is up (asked, not answered yet). Main thread only. */
    var asking = false
        private set

    /** Asks if this Android needs it, the app is connected, and it was neither granted nor asked. */
    fun askIfDue() {
        if (activity.isFinishing || activity.isDestroyed) return
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
