package dev.passporter.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import dev.passporter.app.MainActivity
import dev.passporter.app.R
import java.util.concurrent.ThreadLocalRandom

/**
 * Keeps the built-in transport's stream open while the app is closed and
 * the phone sleeps: a foreground service (type `specialUse`: a persistent
 * connection to your own bridge, no cloud relay) with a quiet, ongoing
 * notification, and one connection thread that reads the stream
 * ([NativeReceiver.listen]) and reconnects with a jittered backoff (1 s
 * doubling to 5 min), waiting for a network when there is none.
 *
 * Started by [BuiltInPush.refresh] while the app is in front, and by
 * [NativePushBootReceiver] after a reboot or an update. It stops itself
 * once the transport is off (not paired, the external distributor chosen,
 * the bridge without a companion). Android may still stop it (a force
 * stop, some vendors' battery savers); opening the app starts it again.
 */
class NativePushService : Service() {
    @Volatile
    private var running = true

    @Volatile
    private var stream: NativeStream? = null

    private var worker: Thread? = null
    private val signal = Object()

    private val network: ConnectivityManager by lazy { getSystemService(ConnectivityManager::class.java) }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = wake()

        override fun onLost(network: Network) = wake()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.push_builtin_channel), NotificationManager.IMPORTANCE_MIN).apply {
                description = getString(R.string.push_builtin_channel_description)
                setShowBadge(false)
            },
        )
        val tap = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).setAction(NotificationSetup.ACTION),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.push_builtin_ongoing))
            .setContentIntent(tap)
            .setOngoing(true)
            .setSilent(true)
            .setPriority(NotificationCompat.PRIORITY_MIN)
            .build()
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
            } else {
                startForeground(ID, notification)
            }
        } catch (e: IllegalStateException) {
            // ForegroundServiceStartNotAllowedException (Android 12+) is one.
            Log.w(PUSH_TAG, "Android refused the built-in transport's service: ${e.javaClass.simpleName}")
            running = false
            stopSelf()
            return
        }
        network.registerDefaultNetworkCallback(networkCallback)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!running) return START_NOT_STICKY
        if (worker?.isAlive != true) {
            worker = Thread(::listen, "passporter-builtin-push").apply {
                isDaemon = true
                start()
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        running = false
        wake()
        try {
            network.unregisterNetworkCallback(networkCallback)
        } catch (e: IllegalArgumentException) {
            // Never registered: the service could not start.
        }
        super.onDestroy()
    }

    /** The connection thread. */
    private fun listen() {
        val receiver = BuiltInPush.receiver(this)
        val power = getSystemService(PowerManager::class.java)
        var failures = 0
        while (running) {
            if (network.activeNetwork == null) {
                NativeStatus.set(NativeState.RetryLater)
                pause(NO_NETWORK_WAIT_MS)
                continue
            }
            val state = receiver.listen(
                running = { running },
                opened = { stream = it },
                connected = {
                    failures = 0
                    NativeStatus.set(NativeState.Connected)
                },
                guard = { work ->
                    val lock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG)
                    lock.acquire(WAKE_LOCK_MS)
                    try {
                        work().also { outcome ->
                            if (outcome !is Ingested.Ignored) {
                                NativeStatus.clockOff = outcome == Ingested.Refused(EnvelopeRefusal.EXPIRED)
                            }
                        }
                    } finally {
                        if (lock.isHeld) lock.release()
                    }
                },
            )
            stream = null
            NativeStatus.set(state)
            if (!running) break
            if (state != NativeState.RetryLater && state != NativeState.Registered) {
                Log.i(PUSH_TAG, "built-in transport stopped: $state")
                // The external registration was dropped when this one took
                // over: say so rather than go quiet until the app is opened.
                if (state == NativeState.BridgeHasNone || state is NativeState.Refused) tellStopped()
                ContextCompat.getMainExecutor(this).execute { stopSelf() }
                break
            }
            failures = (failures + 1).coerceAtMost(MAX_DOUBLINGS)
            val ceiling = (1_000L shl failures).coerceAtMost(MAX_BACKOFF_MS)
            pause(ThreadLocalRandom.current().nextLong(ceiling / 2, ceiling + 1))
        }
    }

    /** A notification that news stopped coming; its tap opens the setup, which falls back. */
    private fun tellStopped() {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(STOPPED_CHANNEL, getString(R.string.push_stopped_channel), NotificationManager.IMPORTANCE_DEFAULT),
        )
        val tap = PendingIntent.getActivity(
            this,
            1,
            Intent(this, MainActivity::class.java).setAction(NotificationSetup.ACTION),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(this, STOPPED_CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.push_stopped_title))
            .setContentText(getString(R.string.push_stopped_text))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .build()
        try {
            manager.notify(STOPPED_ID, notification)
        } catch (e: SecurityException) {
            // Notifications are off: nothing would show anyway.
        }
    }

    private fun pause(ms: Long) = synchronized(signal) {
        if (running) signal.wait(ms)
    }

    /** Ends a wait and the open stream, so the thread reconnects now (or ends). */
    private fun wake() {
        stream?.close()
        synchronized(signal) { signal.notifyAll() }
    }

    companion object {
        private const val CHANNEL = "builtin_push"
        private const val ID = 702
        private const val STOPPED_CHANNEL = "builtin_push_stopped"
        private const val STOPPED_ID = 703
        private const val WAKE_LOCK_TAG = "Passporter:notice"
        private const val WAKE_LOCK_MS = 10_000L
        private const val NO_NETWORK_WAIT_MS = 60_000L
        private const val MAX_BACKOFF_MS = 300_000L
        private const val MAX_DOUBLINGS = 9

        /**
         * Starts the service (a no-op if it runs); whether Android let it.
         * Only while the app is in front, or from [NativePushBootReceiver];
         * refused otherwise, which is logged.
         */
        fun start(context: Context): Boolean = try {
            ContextCompat.startForegroundService(context, Intent(context, NativePushService::class.java))
            true
        } catch (e: IllegalStateException) {
            Log.w(PUSH_TAG, "could not start the built-in transport: ${e.javaClass.simpleName}")
            false
        }

        /** Stops the service, if it runs. */
        fun stop(context: Context) {
            context.stopService(Intent(context, NativePushService::class.java))
        }
    }
}

/**
 * Starts the built-in transport again after a reboot (once the phone is
 * first unlocked) and after an app update, if it was running. A force
 * stop is not undone: Android leaves that to you.
 */
class NativePushBootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED && intent.action != Intent.ACTION_MY_PACKAGE_REPLACED) return
        if (BuiltInPush.active(context)) NativePushService.start(context)
    }
}
