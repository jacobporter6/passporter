package dev.passporter.recorder

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat

/**
 * The foreground service (type `microphone`) that keeps a take recording
 * with the screen off, the app in the background or its task swiped away.
 * Android lets an app use the microphone in the background only from such
 * a service, started while the app is in the foreground: [Recorder.start]
 * runs from the PWA's tap, so it is.
 *
 * It shows an ongoing notification with the time recorded and a Stop
 * action; a tap on it opens the app. It goes as soon as the take ends,
 * however it ends ([Recorder.watchIdle]). It never starts a take itself,
 * and Android does not restart it (`START_NOT_STICKY`): a process that died
 * mid-take leaves the take to [TakeStore.open].
 */
class RecorderService : Service() {
    private val main = Handler(Looper.getMainLooper())

    /** The start that began the latest take; main thread only. */
    private var latestStart = 0

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            // The notification's Stop: the service is in the foreground
            // already, and goes once the take has ended.
            Recorder.worker.execute { Recorder.get(this).stopActive(EndReason.USER) }
            return START_NOT_STICKY
        }
        latestStart = startId
        // First: a service started with startForegroundService must call
        // startForeground in time, even if its take is already over.
        try {
            ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(), FOREGROUND_TYPE)
        } catch (e: RuntimeException) {
            // Not allowed now (Android 12+ background-start rules, or no
            // microphone permission): end the take, keeping what was said.
            Log.e(TAG, "the recording service could not go to the foreground", e)
            Recorder.worker.execute { Recorder.get(this).stopActive(EndReason.ERROR) }
            stopSelf(startId)
            return START_NOT_STICKY
        }
        Recorder.worker.execute {
            val watching = Recorder.get(this).watchIdle { leave(startId) }
            if (!watching) leave(startId)
        }
        return START_NOT_STICKY
    }

    /**
     * Leaves the foreground and stops, unless a newer take started
     * meanwhile (its start keeps the service, under a newer [startId]).
     * Runs on [Recorder.worker].
     */
    private fun leave(startId: Int) {
        if (Recorder.get(this).isActive()) return
        main.post {
            if (startId != latestStart) return@post
            ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
            stopSelf(startId)
        }
    }

    private fun notification() = NotificationCompat.Builder(this, ensureChannel(this))
        .setSmallIcon(R.drawable.ic_recorder_mic)
        .setContentTitle(getString(R.string.recorder_recording))
        .setContentText(getString(R.string.recorder_recording_detail))
        .setOngoing(true)
        .setOnlyAlertOnce(true)
        .setSilent(true)
        .setCategory(NotificationCompat.CATEGORY_SERVICE)
        .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
        .setUsesChronometer(true)
        .setWhen(System.currentTimeMillis())
        .setShowWhen(true)
        .apply { openApp()?.let(::setContentIntent) }
        .addAction(0, getString(R.string.recorder_stop), stopIntent())
        .build()

    /** The Stop action: this service, explicitly, with [ACTION_STOP]. */
    private fun stopIntent(): PendingIntent = PendingIntent.getService(
        this,
        0,
        Intent(this, RecorderService::class.java).setAction(ACTION_STOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    /** A tap opens the app where it was (its launcher activity). */
    private fun openApp(): PendingIntent? = packageManager.getLaunchIntentForPackage(packageName)?.let { launch ->
        PendingIntent.getActivity(this, 0, launch, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    }

    companion object {
        private const val TAG = "PassporterRecorder"

        /** The notification's id; agent notifications use their own ids (`dev.passporter.notify`). */
        internal const val NOTIFICATION_ID = 0x7EC0

        /** The notification's Stop. */
        internal const val ACTION_STOP = "dev.passporter.recorder.STOP"

        /** The channel: low importance, no sound; it only says a take is recording. */
        internal const val CHANNEL = "dictation"

        private val FOREGROUND_TYPE =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE else 0

        /** Starts the service for the take that just started. Throws when Android refuses. */
        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, RecorderService::class.java))
        }

        private fun ensureChannel(context: Context): String {
            val manager = context.getSystemService(NotificationManager::class.java)
            if (manager.getNotificationChannel(CHANNEL) == null) {
                val channel = NotificationChannel(CHANNEL, context.getString(R.string.recorder_channel), NotificationManager.IMPORTANCE_LOW)
                channel.description = context.getString(R.string.recorder_channel_description)
                channel.setShowBadge(false)
                manager.createNotificationChannel(channel)
            }
            return CHANNEL
        }
    }
}
