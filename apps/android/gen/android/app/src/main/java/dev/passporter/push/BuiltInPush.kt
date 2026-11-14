package dev.passporter.push

import android.content.Context
import android.util.Log
import dev.passporter.device.DeviceAuth
import dev.passporter.notify.NoticeNotifier
import java.io.File
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
 *   the [NativePushService] started, and any external registration dropped
 *   ([PushSync.leave]) so the bridge sends each notice once;
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

    @Volatile
    private var receiver: NativeReceiver? = null

    /** The app's one [NativeReceiver]. Any context will do; the application context is kept. */
    fun receiver(context: Context): NativeReceiver = receiver ?: synchronized(this) {
        receiver ?: context.applicationContext.let { app ->
            val auth = DeviceAuth.get(app)
            NativeReceiver(
                store = NativeStore(File(app.noBackupFilesDir, RECORD_FILE)),
                bridge = DeviceAuthNativeBridge(app, auth),
                keys = DefaultKeyManager(app),
                show = { owner, text -> NoticeNotifier(app) { owner }.show(text) },
                current = auth::savedDevice,
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
        NativeStatus.set(state)
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
        NativePushService.start(app)
        val left = PushWork.sync(app).leave()
        Log.i(PUSH_TAG, "built-in transport: $state; external registration: $left")
        return Settled.BUILT_IN
    }

    /** Whether the built-in transport is what brings news now (no external registration wanted). */
    fun active(context: Context): Boolean {
        val record = receiver(context).record()
        return record.choice == Transport.BUILT_IN && record.registered
    }
}

/**
 * The built-in transport's last state, for the status the PWA shows
 * (`NotificationStatus`). In memory: the service runs in the app's process.
 */
object NativeStatus {
    @Volatile
    private var state: NativeState? = null

    /** The last state, or null before anything was tried in this process. */
    fun get(): NativeState? = state

    /** Records [next]. */
    fun set(next: NativeState) {
        state = next
    }
}
