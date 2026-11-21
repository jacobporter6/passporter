package dev.passporter.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.app.NotificationCompat
import dev.passporter.app.MainActivity
import dev.passporter.app.R
import dev.passporter.device.DeviceAuth
import dev.passporter.notify.NoticeNotifier
import dev.passporter.notify.actions.PairedAs
import java.io.File
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.keys.DefaultKeyManager

/** Which transport [BuiltInPush.refresh] settled on. */
enum class Settled {
    /** The built-in transport runs; the external registration is dropped. */
    BUILT_IN,

    /** The external distributor's turn (`PushRegistration`, as before). */
    EXTERNAL,
}

/**
 * Chooses between the two transports and keeps the built-in one running
 * (design §6.6), on [PushWork]'s thread:
 *
 * - by default the bridge's own transport, when it has one: registered,
 *   the [NativePushService] started, and the external registration kept
 *   (made for a new pairing, [Interim]) until its stream first connects,
 *   then dropped ([PushSync.leave]), so the bridge sends each notice once
 *   and a stream that never connects loses nothing;
 * - the external distributor (T32a's path, unchanged) when the bridge has
 *   no companion, refuses the registration, or you chose it
 *   ([Transport.EXTERNAL]).
 *
 * A notice that arrives both ways while switching is shown once: the
 * bridge gives all of a device's subscriptions the same ping, which the
 * notifier remembers.
 */
object BuiltInPush {
    /** The record's file in `noBackupFilesDir`. */
    private const val RECORD_FILE = "passporter-builtin-push.json"

    private const val STOPPED_CHANNEL = "builtin_push_stopped"
    private const val STOPPED_ID = 703

    @Volatile
    private var receiver: NativeReceiver? = null

    /** The app's one [NativeReceiver]. Any context will do; the application context is kept. */
    fun receiver(context: Context): NativeReceiver = receiver ?: synchronized(this) {
        receiver ?: context.applicationContext.let { app ->
            val auth = DeviceAuth.get(app)
            // The saved record only: no Keystore call per message.
            val paired = { auth.savedDevice()?.let(PairedAs::Device) ?: PairedAs.Nothing }
            NativeReceiver(
                store = NativeStore(File(app.noBackupFilesDir, RECORD_FILE)),
                bridge = DeviceAuthNativeBridge(app, auth),
                keys = DefaultKeyManager(app),
                show = { owner, text -> NoticeNotifier(app, { owner }, paired).show(text) },
                paired = paired,
            )
        }.also { receiver = it }
    }

    /**
     * Registers the built-in transport if it is the choice and the bridge
     * has it, and starts or stops the service to match. Call it on
     * [PushWork]'s thread while the app is in front (starting a foreground
     * service from the background is not allowed).
     */
    fun refresh(context: Context): Settled {
        val app = context.applicationContext
        val receiver = receiver(app)
        val state = receiver.register()
        NativeStatus.registered(state)
        val builtIn = when (state) {
            NativeState.Registered, NativeState.Connected -> true
            // Kept while the bridge is out of reach, if it worked before.
            NativeState.RetryLater -> receiver.record().registered
            NativeState.NotPaired, NativeState.Off, NativeState.BridgeHasNone, is NativeState.Refused -> false
        }
        if (!builtIn) {
            NativePushService.stop(app)
            return Settled.EXTERNAL
        }
        // Android refuses the start if the app went to the background meanwhile.
        // The external registration is dropped by the service itself, once
        // its stream connects.
        if (!NativePushService.start(app)) return Settled.EXTERNAL
        Log.i(PUSH_TAG, "built-in transport: $state")
        return Settled.BUILT_IN
    }

    /**
     * The bridge's VAPID key to register the saved distributor with while
     * the built-in transport is settled on but its stream has never
     * connected for this pairing ([Interim]), or null. On [PushWork]'s thread.
     */
    fun interimVapidKey(context: Context): String? {
        val app = context.applicationContext
        val connected = receiver(app).record().streamConnected
        return Interim.vapidKey(connected, UnifiedPush.getSavedDistributor(app)) { PushWork.sync(app).readiness() }
    }

    /** Whether the built-in transport is the choice and registered (what starts it after a reboot). */
    fun active(context: Context): Boolean {
        val record = receiver(context).record()
        return record.choice == Transport.BUILT_IN && record.registered
    }

    /**
     * Whether the built-in transport brings news now: chosen (a durable
     * choice, so a switch to the ntfy app counts at once), registered, and
     * its stream connected. An external endpoint is not wanted then.
     */
    fun bringsNews(context: Context): Boolean = active(context) && NativeStatus.get() == NativeState.Connected

    /**
     * The built-in transport stopped (or could not start) while the app may
     * be in the background: registers with the distributor you chose
     * before, if the bridge allows it, so news keeps coming. Otherwise (no
     * saved distributor, which needs the app's picker, or the bridge says
     * no) it posts that agent news stopped. [done] runs afterwards.
     */
    fun fallBack(context: Context, done: () -> Unit = {}) {
        val app = context.applicationContext
        PushWork.run {
            try {
                val distributor = UnifiedPush.getSavedDistributor(app)
                when (val fallback = Fallback.decide(distributor) { PushWork.sync(app).readiness() }) {
                    is Fallback.Register -> {
                        UnifiedPush.register(app, PushWork.INSTANCE, app.getString(R.string.app_name), fallback.vapidKey)
                        Log.i(PUSH_TAG, "fell back to the distributor ($distributor)")
                    }
                    is Fallback.Tell -> {
                        Log.i(PUSH_TAG, "no fallback to the distributor: ${fallback.readiness}")
                        tellStopped(app)
                    }
                    Fallback.NoDistributor -> {
                        Log.i(PUSH_TAG, "no fallback: no distributor saved")
                        tellStopped(app)
                    }
                }
            } finally {
                done()
            }
        }
    }

    /**
     * A notification that nothing brings news any more (the built-in
     * transport stopped and the distributor could not take over); its tap
     * opens the setup.
     */
    private fun tellStopped(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(STOPPED_CHANNEL, context.getString(R.string.push_stopped_channel), NotificationManager.IMPORTANCE_DEFAULT),
        )
        val tap = PendingIntent.getActivity(
            context,
            1,
            Intent(context, MainActivity::class.java).setAction(NotificationSetup.ACTION),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(context, STOPPED_CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(context.getString(R.string.push_stopped_title))
            .setContentText(context.getString(R.string.push_stopped_text))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .build()
        try {
            manager.notify(STOPPED_ID, notification)
        } catch (e: SecurityException) {
            // Notifications are off: nothing would show anyway.
        }
    }
}

/** What [BuiltInPush.fallBack] does once the built-in transport stopped. */
internal sealed interface Fallback {
    /** Register with the saved distributor, for the bridge's [vapidKey]. */
    data class Register(
        /** The bridge's VAPID public key. */
        val vapidKey: String,
    ) : Fallback

    /** Post that agent news stopped: the saved distributor cannot take over, the bridge is not ready. */
    data class Tell(
        /** The bridge's answer. */
        val readiness: Readiness,
    ) : Fallback

    /** Post that agent news stopped: no distributor is saved (choosing one needs the app's picker). */
    data object NoDistributor : Fallback

    companion object {
        /**
         * The fallback for the saved [distributor] (null if none): only it can
         * take over without the app's picker, and only if the bridge is ready
         * ([readiness], asked only when one is saved).
         */
        fun decide(distributor: String?, readiness: () -> Readiness): Fallback {
            if (distributor == null) return NoDistributor
            return when (val ready = readiness()) {
                is Readiness.Register -> Register(ready.vapidKey)
                else -> Tell(ready)
            }
        }
    }
}

/**
 * The external registration while the built-in transport is settled on
 * but its stream has never connected for the current pairing
 * ([NativeRecord.streamConnected], forgotten with a new pairing). It
 * stays, or after a new pairing is made for the new device, so a stream
 * that never connects (the companion down, a proxy that ends streams)
 * still leaves a transport. The service drops it once the stream
 * connects, and an endpoint that arrives after that is dropped too.
 */
internal object Interim {
    /**
     * The VAPID key to register the saved [distributor] with, or null: none
     * once the stream has [connected] for this pairing, none without a saved distributor (the
     * picker is not opened for a transport that is not the choice), none
     * unless the bridge is ready ([readiness], asked only otherwise).
     */
    fun vapidKey(connected: Boolean, distributor: String?, readiness: () -> Readiness): String? =
        if (connected) null else (Fallback.decide(distributor, readiness) as? Fallback.Register)?.vapidKey
}

/**
 * The built-in transport's last state, for the status the PWA shows
 * (`NotificationStatus`). In memory: the service runs in the app's process.
 */
object NativeStatus {
    @Volatile
    private var state: NativeState? = null

    /**
     * Whether the last message was refused for its timing: the phone's
     * clock is off, so every notice is.
     */
    @Volatile
    var clockOff: Boolean = false

    /** Forgets what it knew (a pairing change). */
    fun reset() {
        state = null
        clockOff = false
    }

    /** The last state, or null before anything was tried in this process. */
    fun get(): NativeState? = state

    /** Records [next], what the service saw. */
    fun set(next: NativeState) {
        state = next
    }

    /**
     * Records [next], a registration's outcome: a confirmed registration
     * does not hide that the service's stream is open.
     */
    fun registered(next: NativeState) {
        if (next == NativeState.Registered && state == NativeState.Connected) return
        state = next
    }
}
