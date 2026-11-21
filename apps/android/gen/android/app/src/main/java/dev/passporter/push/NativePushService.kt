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

    /** The id of the last start ([stopUnlessStartedAgain]). */
    @Volatile
    private var lastStart = 0
    private val signal = Object()

    /**
     * Set by [wake], cleared by [pause] and before each connection: a wake
     * during a connection is not lost, one before it does not skip the
     * backoff after it. Guarded by [signal].
     */
    private var woken = false

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
            BuiltInPush.fallBack(this)
            stopSelf()
            return
        }
        network.registerDefaultNetworkCallback(networkCallback)
        // A registration replaced under the open stream (a new pairing) ends it.
        // The stream was the old registration's: not connected now, so a new
        // pairing's external endpoint is kept until the new stream connects.
        BuiltInPush.receiver(this).onRegistrationChanged = {
            NativeStatus.set(NativeState.RetryLater)
            wake()
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!running) return START_NOT_STICKY
        lastStart = startId
        ensureWorker()
        return START_STICKY
    }

    /**
     * Starts the connection thread unless one runs. Main thread. A thread
     * that has decided to end clears [worker] first, so it never counts.
     */
    private fun ensureWorker() {
        if (worker?.isAlive == true) return
        worker = Thread(::listen, "passporter-builtin-push").apply {
            isDaemon = true
            start()
        }
    }

    /**
     * Stops the service, unless a start came after [startId] (a refresh
     * racing the thread's end): then listening goes on. Main thread.
     */
    private fun stopUnlessStartedAgain(startId: Int) {
        if (!stopSelfResult(startId)) ensureWorker()
    }

    override fun onDestroy() {
        BuiltInPush.receiver(this).onRegistrationChanged = null
        running = false
        wake()
        try {
            network.unregisterNetworkCallback(networkCallback)
        } catch (e: IllegalArgumentException) {
            // Never registered: the service could not start.
        }
        super.onDestroy()
    }

    /**
     * The connection thread. It never dies of an exception: one from a
     * connection is retried after the backoff ([connect]), anything else
     * ends the thread and the service as a stop would.
     */
    private fun listen() {
        try {
            connect()
        } catch (e: RuntimeException) {
            Log.e(PUSH_TAG, "the built-in transport's thread failed: ${e.javaClass.name}")
            end()
        }
    }

    /** Connects, reads and reconnects until the service stops or the transport is off. */
    private fun connect() {
        val receiver = BuiltInPush.receiver(this)
        val power = getSystemService(PowerManager::class.java)
        var failures = 0
        while (running) {
            if (network.activeNetwork == null) {
                NativeStatus.set(NativeState.RetryLater)
                pause(NO_NETWORK_WAIT_MS)
                continue
            }
            // This connection answers any wake so far (the network callback's first
            // onAvailable among them): only one during it skips the next backoff.
            synchronized(signal) { woken = false }
            val state = try {
                listenOnce(receiver, power) { failures = 0 }
            } catch (e: RuntimeException) {
                // A Keystore or platform failure outside a message: reconnect after the backoff.
                Log.e(PUSH_TAG, "the built-in stream failed: ${e.javaClass.name}")
                NativeState.RetryLater
            }
            stream = null
            NativeStatus.set(state)
            if (!running) break
            if (state != NativeState.RetryLater && state != NativeState.Registered) {
                Log.i(PUSH_TAG, "built-in transport stopped: $state")
                // The external registration was dropped when this one took
                // over: say so rather than go quiet until the app is opened.
                if (state == NativeState.BridgeHasNone || state is NativeState.Refused) BuiltInPush.fallBack(this)
                end()
                break
            }
            failures = (failures + 1).coerceAtMost(MAX_DOUBLINGS)
            val ceiling = (1_000L shl failures).coerceAtMost(MAX_BACKOFF_MS)
            pause(ThreadLocalRandom.current().nextLong(ceiling / 2, ceiling + 1))
        }
    }

    /** One connection ([NativeReceiver.listen]); [onConnected] once it is open. */
    private fun listenOnce(receiver: NativeReceiver, power: PowerManager, onConnected: () -> Unit): NativeState =
        receiver.listen(
            running = { running },
            opened = { stream = it },
            connected = {
                onConnected()
                NativeStatus.set(NativeState.Connected)
                // News arrives this way now: the external registration can go
                // (kept until here, so a stream that never connects loses nothing).
                val app = applicationContext
                PushWork.run {
                    // Not if you switched to the ntfy app meanwhile.
                    if (BuiltInPush.bringsNews(app)) {
                        Log.i(PUSH_TAG, "external registration: ${PushWork.sync(app).leave()}")
                    }
                }
            },
            guard = { work ->
                val lock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG)
                lock.acquire(WAKE_LOCK_MS)
                try {
                    work().also { outcome ->
                        if (outcome !is Ingested.Ignored) {
                            NativeStatus.clockOff = outcome == Ingested.Refused(EnvelopeRefusal.CLOCK)
                        }
                    }
                } finally {
                    if (lock.isHeld) lock.release()
                }
            },
        )

    /** Ends the connection thread (the caller's) and stops the service, unless started again. */
    private fun end() {
        val startId = lastStart
        val me = Thread.currentThread()
        ContextCompat.getMainExecutor(this).execute {
            // This thread is ending: a start after startId gets a fresh one.
            if (worker === me) worker = null
            stopUnlessStartedAgain(startId)
        }
    }

    private fun pause(ms: Long) = synchronized(signal) {
        // A wake that came before the wait still counts.
        if (running && !woken) signal.wait(ms)
        woken = false
    }

    /** Ends a wait and the open stream, so the thread reconnects now (or ends). */
    private fun wake() {
        stream?.close()
        synchronized(signal) {
            woken = true
            signal.notifyAll()
        }
    }

    companion object {
        private const val CHANNEL = "builtin_push"
        private const val ID = 702
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
        if (!BuiltInPush.active(context) || NativePushService.start(context)) return
        // Kept alive until the fallback (a bridge request) is done.
        val pending = goAsync()
        BuiltInPush.fallBack(context) { pending.finish() }
    }
}
