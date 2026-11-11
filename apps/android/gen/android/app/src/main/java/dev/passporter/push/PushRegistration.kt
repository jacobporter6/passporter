package dev.passporter.push

import android.content.Context
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.appcompat.app.AlertDialog
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import dev.passporter.app.R
import dev.passporter.device.DeviceAuth
import dev.passporter.device.PairingListener
import org.unifiedpush.android.connector.UnifiedPush

/**
 * Registers the app with the phone's UnifiedPush distributor (ntfy, say)
 * while it is paired, from the activity (design §6.6):
 *
 * - whenever the activity resumes, and right after pairing, it asks the
 *   bridge for its VAPID key and registers (again: the distributor answers
 *   with the same endpoint, which [PushSync] has posted already);
 * - with no distributor saved, it uses the phone's default one, which may
 *   show Android's picker (once per activity) when there are several;
 * - with none installed, it says so once, in a dialog;
 * - after the pairing is forgotten, [PushSync] unregisters.
 *
 * Attach it ([attachTo]) while the activity is being constructed or created.
 */
class PushRegistration private constructor(private val activity: ComponentActivity) {
    private val app: Context = activity.applicationContext

    /** Whether this activity showed the distributor picker already. */
    private var pickerShown = false

    private val onPairing = PairingListener { status ->
        if (status.paired) activity.runOnUiThread(::refresh) else PushWork.run { PushWork.sync(app).sync() }
    }

    init {
        activity.lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onCreate(owner: LifecycleOwner) = DeviceAuth.get(app).addPairingListener(onPairing)
            override fun onResume(owner: LifecycleOwner) = refresh()
            override fun onDestroy(owner: LifecycleOwner) = DeviceAuth.get(app).removePairingListener(onPairing)
        })
    }

    /** Registers if the app is paired and the bridge has push on. */
    private fun refresh() {
        PushWork.run {
            when (val readiness = PushWork.sync(app).readiness()) {
                is Readiness.Register -> register(readiness.vapidKey)
                else -> Log.i(TAG, "not registering for push: $readiness")
            }
        }
    }

    /** On the push thread: registers with the saved distributor, or finds one. */
    private fun register(vapidKey: String) {
        when {
            UnifiedPush.getSavedDistributor(app) != null -> registerNow(vapidKey)
            UnifiedPush.getDistributors(app).none { it != app.packageName } ->
                activity.runOnUiThread(::noDistributor)
            else -> activity.runOnUiThread { pickDistributor(vapidKey) }
        }
    }

    /** On the main thread: the phone's default distributor, or Android's picker. */
    private fun pickDistributor(vapidKey: String) {
        if (pickerShown || activity.isFinishing) return
        pickerShown = true
        UnifiedPush.tryUseDefaultDistributor(activity) { chosen ->
            if (chosen) PushWork.run { registerNow(vapidKey) } else Log.i(TAG, "no distributor chosen")
        }
    }

    private fun registerNow(vapidKey: String) {
        try {
            UnifiedPush.register(app, PushWork.INSTANCE, app.getString(R.string.app_name), vapidKey)
        } catch (e: UnifiedPush.VapidNotValidException) {
            // VapidKey checked its shape already.
            Log.e(TAG, "the bridge's VAPID key was refused", e)
        }
    }

    /** On the main thread: tells the person, once ever, that push needs a distributor. */
    private fun noDistributor() {
        val preferences = app.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        Log.i(TAG, "no UnifiedPush distributor installed")
        if (preferences.getBoolean(TOLD, false) || activity.isFinishing) return
        preferences.edit().putBoolean(TOLD, true).apply()
        AlertDialog.Builder(activity)
            .setTitle(R.string.push_no_distributor_title)
            .setMessage(R.string.push_no_distributor_message)
            .setPositiveButton(android.R.string.ok, null)
            .show()
    }

    companion object {
        /** Registers for push on [activity]'s behalf for as long as it lives. */
        fun attachTo(activity: ComponentActivity) {
            PushRegistration(activity)
        }

        private const val TAG = "PassporterPush"
        private const val PREFERENCES = "dev.passporter.push"
        private const val TOLD = "toldNoDistributor"
    }
}
