package dev.passporter.push

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.appcompat.app.AlertDialog
import androidx.core.app.NotificationManagerCompat
import dev.passporter.app.R
import dev.passporter.device.DeviceAuth
import dev.passporter.device.NotificationStatus

/**
 * The app's own notification setup: the status the PWA's settings show
 * ([status]), and a native dialog ([open]) from the PWA's *Set up* button,
 * the launcher's *Notifications* shortcut or the built-in transport's
 * ongoing notification. In it you retry, switch between the built-in
 * transport and the ntfy app, and let the app run in the background.
 */
class NotificationSetup(
    private val activity: ComponentActivity,
    /** Registers again (`PushRegistration.refresh`). */
    private val refresh: () -> Unit,
) {
    private val app: Context by lazy { activity.applicationContext }

    /** The status now; reads files, so off the main thread. */
    fun status(): NotificationStatus {
        val paired = DeviceAuth.get(app).savedDevice()
            ?: return NotificationStatus(false, app.getString(R.string.push_status_not_paired))
        if (!NotificationManagerCompat.from(app).areNotificationsEnabled()) {
            return NotificationStatus(false, app.getString(R.string.push_status_blocked))
        }
        val record = BuiltInPush.receiver(app).record()
        val external = PushWork.store(app).load().subscribed?.deviceId == paired.value
        val state = NativeStatus.get()
        return when {
            record.choice == Transport.EXTERNAL ->
                if (external) status(true, R.string.push_status_external) else status(false, R.string.push_status_external_waiting)
            record.registered && record.owner == paired.value -> when (state) {
                NativeState.Connected -> status(true, R.string.push_status_builtin_connected)
                NativeState.RetryLater -> status(false, R.string.push_status_builtin_retrying)
                else -> status(false, R.string.push_status_builtin_connecting)
            }
            state == NativeState.BridgeHasNone || state is NativeState.Refused ->
                if (external) status(true, R.string.push_status_external_fallback) else status(false, R.string.push_status_none)
            else -> status(false, R.string.push_status_builtin_connecting)
        }
    }

    private fun status(connected: Boolean, message: Int) = NotificationStatus(connected, app.getString(message))

    /** Opens the dialog. Main thread. */
    fun open() {
        if (!NotificationManagerCompat.from(app).areNotificationsEnabled()) return blocked()
        PushWork.run {
            val status = status()
            val choice = BuiltInPush.receiver(app).record().choice
            activity.runOnUiThread { if (!activity.isFinishing && !activity.isDestroyed) show(status, choice) }
        }
    }

    private fun show(status: NotificationStatus, choice: Transport) {
        val builtIn = choice == Transport.BUILT_IN
        val restricted = builtIn && !unrestricted()
        val message = if (restricted) status.message + "\n\n" + app.getString(R.string.push_setup_background) else status.message
        val dialog = AlertDialog.Builder(activity)
            .setTitle(R.string.push_setup_title)
            .setMessage(message)
            .setPositiveButton(R.string.push_setup_retry) { _, _ -> refresh() }
            .setNeutralButton(if (builtIn) R.string.push_setup_use_external else R.string.push_setup_use_builtin) { _, _ ->
                val next = if (builtIn) Transport.EXTERNAL else Transport.BUILT_IN
                PushWork.run { BuiltInPush.receiver(app).choose(next) }
                refresh()
            }
        if (restricted) {
            dialog.setNegativeButton(R.string.push_setup_allow_background) { _, _ -> askUnrestricted() }
        } else {
            dialog.setNegativeButton(android.R.string.cancel, null)
        }
        dialog.show()
    }

    /**
     * Once ever, after the built-in transport first works, asks to let the
     * app run in the background: without it Doze may hold the stream for
     * minutes. Main thread.
     */
    fun offerBackgroundOnce() {
        if (activity.isFinishing || activity.isDestroyed || unrestricted()) return
        val preferences = app.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        if (preferences.getBoolean(OFFERED, false)) return
        preferences.edit().putBoolean(OFFERED, true).apply()
        AlertDialog.Builder(activity)
            .setTitle(R.string.push_background_title)
            .setMessage(R.string.push_setup_background)
            .setPositiveButton(R.string.push_setup_allow_background) { _, _ -> askUnrestricted() }
            .setNegativeButton(R.string.push_background_later, null)
            .show()
    }

    private fun unrestricted(): Boolean =
        app.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(app.packageName)

    /** Android's own prompt to exempt the app from battery optimization. */
    private fun askUnrestricted() {
        val ask = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${app.packageName}"))
        try {
            activity.startActivity(ask)
        } catch (e: ActivityNotFoundException) {
            // Some vendors leave the prompt out: their list of apps instead.
            activity.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        }
    }

    /** Notifications are off for the app: only Android's settings can turn them on. */
    private fun blocked() {
        AlertDialog.Builder(activity)
            .setTitle(R.string.push_setup_title)
            .setMessage(R.string.push_status_blocked)
            .setPositiveButton(R.string.push_setup_open_settings) { _, _ ->
                activity.startActivity(
                    Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, app.packageName),
                )
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    companion object {
        /** Intent action that opens the dialog (the *Notifications* shortcut, the ongoing notification). */
        const val ACTION = "dev.passporter.app.NOTIFICATIONS"

        private const val PREFERENCES = "dev.passporter.push.builtin"
        private const val OFFERED = "offeredBackground"
    }
}
