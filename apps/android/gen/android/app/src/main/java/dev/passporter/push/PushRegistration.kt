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
import org.unifiedpush.android.connector.data.ResolvedDistributor

/**
 * Registers the app with the phone's UnifiedPush distributor (ntfy, say)
 * while it is paired and the bridge's built-in transport is not the one
 * ([BuiltInPush], tried first), from the activity (design §6.6):
 *
 * - whenever the activity resumes, and right after pairing, it asks
 *   [PushSync] whether to register (paired, and the bridge has push on)
 *   and registers again: the distributor answers with the same endpoint,
 *   which [PushSync] has posted already;
 * - with no distributor saved, it uses the phone's default one; when there
 *   are several and none is the default, Android's picker opens, at most
 *   once per activity and pairing;
 * - with none installed, it says so once, in a dialog;
 * - after the pairing is forgotten, [PushSync] unregisters.
 *
 * The picker and the dialog wait while another prompt is up ([promptsHeld]:
 * the notification permission's); the next [refresh] shows them.
 *
 * Removing Passporter inside the distributor lasts only until the app
 * next opens; Android's notification settings for the app are the way to
 * silence it.
 *
 * Attach it ([attachTo]) while the activity is being constructed or created.
 */
class PushRegistration private constructor(
    private val activity: ComponentActivity,
    private val promptsHeld: () -> Boolean,
    private val onBuiltIn: () -> Unit,
) {
    /** Read once the activity is created: in its constructor it has no context yet. */
    private val app: Context by lazy { activity.applicationContext }

    /** Whether Android's distributor picker opened already for this activity and pairing. */
    @Volatile
    private var pickerShown = false

    private val onPairing = PairingListener {
        // The status may be stale by now: PushSync reads the pairing again.
        pickerShown = false
        activity.runOnUiThread(::refresh)
    }

    init {
        activity.lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onCreate(owner: LifecycleOwner) = DeviceAuth.get(app).addPairingListener(onPairing)
            override fun onResume(owner: LifecycleOwner) = refresh()
            override fun onDestroy(owner: LifecycleOwner) = DeviceAuth.get(app).removePairingListener(onPairing)
        })
    }

    /**
     * Registers if the app is paired and the bridge has push on; unpaired,
     * leaves. Runs on every resume and pairing; call it also when a prompt
     * that held this one's back is answered.
     */
    fun refresh() {
        val app = app
        PushWork.run {
            // The bridge's own transport first: with it, no distributor.
            if (BuiltInPush.refresh(app) == Settled.BUILT_IN) {
                onActivity(onBuiltIn)
                return@run
            }
            when (val readiness = PushWork.sync(app).readiness()) {
                is Readiness.Register -> register(readiness.vapidKey)
                else -> Log.i(PUSH_TAG, "not registering for push: $readiness")
            }
        }
    }

    /** On the push thread: registers with the saved distributor, or finds one. */
    private fun register(vapidKey: String) {
        if (UnifiedPush.getSavedDistributor(app) != null) return registerNow(vapidKey)
        when (val resolved = UnifiedPush.resolveDefaultDistributor(app)) {
            is ResolvedDistributor.Found -> {
                UnifiedPush.saveDistributor(app, resolved.packageName)
                registerNow(vapidKey)
            }
            ResolvedDistributor.ToSelect -> onActivity { pickDistributor(vapidKey) }
            ResolvedDistributor.NoneAvailable -> onActivity(::noDistributor)
        }
    }

    /** On the main thread: Android's picker, once. */
    private fun pickDistributor(vapidKey: String) {
        if (pickerShown) return
        pickerShown = true
        UnifiedPush.tryUseDefaultDistributor(activity) { chosen ->
            if (chosen) PushWork.run { registerNow(vapidKey) } else Log.i(PUSH_TAG, "no distributor chosen")
        }
    }

    private fun registerNow(vapidKey: String) {
        try {
            UnifiedPush.register(app, PushWork.INSTANCE, app.getString(R.string.app_name), vapidKey)
        } catch (e: UnifiedPush.VapidNotValidException) {
            // VapidKey checked its shape already.
            Log.e(PUSH_TAG, "the bridge's VAPID key was refused", e)
        }
    }

    /** On the main thread: tells the person, once ever, that push needs a distributor. */
    private fun noDistributor() {
        Log.i(PUSH_TAG, "no UnifiedPush distributor installed")
        val preferences = app.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        if (preferences.getBoolean(TOLD, false)) return
        AlertDialog.Builder(activity)
            .setTitle(R.string.push_no_distributor_title)
            .setMessage(R.string.push_no_distributor_message)
            .setPositiveButton(android.R.string.ok, null)
            .show()
        preferences.edit().putBoolean(TOLD, true).apply()
    }

    /**
     * Runs [work], a prompt, on the main thread if the activity is still
     * there by then and no other prompt is up.
     */
    private fun onActivity(work: () -> Unit) {
        activity.runOnUiThread {
            when {
                activity.isFinishing || activity.isDestroyed -> Unit
                promptsHeld() -> Log.i(PUSH_TAG, "another prompt is up; push asks later")
                else -> work()
            }
        }
    }

    companion object {
        /**
         * Registers for push on [activity]'s behalf for as long as it lives;
         * its prompts wait while [promptsHeld]. [onBuiltIn] runs on the main
         * thread when the built-in transport is the one (and no prompt is up).
         */
        fun attachTo(activity: ComponentActivity, promptsHeld: () -> Boolean, onBuiltIn: () -> Unit): PushRegistration =
            PushRegistration(activity, promptsHeld, onBuiltIn)

        private const val PREFERENCES = "dev.passporter.push"
        private const val TOLD = "toldNoDistributor"
    }
}
