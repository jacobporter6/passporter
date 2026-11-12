package dev.passporter.app

import android.app.NotificationManager
import android.content.Context
import android.util.Log
import androidx.work.WorkManager
import dev.passporter.car.CarOutbox
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NotificationLock
import dev.passporter.notify.actions.Outbox
import dev.passporter.notify.actions.PreferenceFrozenBodies
import dev.passporter.recorder.Recorder

/**
 * Drops what the app keeps for one pairing, once the pairing or the bridge
 * changes: after every pairing and forget ([PassporterApplication]'s
 * pairing listener), *Change address* included ([ShellPlugin.restart]). Agent
 * ids repeat across bridges (`w1:p1`), so none of it may reach the next
 * one:
 *
 * - the queued answers, from the shade ([Outbox]) and the car
 *   ([CarOutbox]), and the frozen bodies of the shade's;
 * - the agents' notifications, whose actions would answer the new bridge
 *   and whose news the reconciler would check against its agents (the
 *   dictation recorder's own notification stays);
 * - the dictation takes nobody has released ([Recorder.discardAll]),
 *   which the new bridge's PWA would adopt.
 *
 * Each answer also carries the device it was made for, and is never sent
 * once the app is paired as another (`dev.passporter.notify.actions.DeviceStamp`):
 * this reset is what keeps the shade clear, that check what holds if it
 * fails. Idempotent; each step is tried even if one before it failed.
 * Blocking: call it off the main thread.
 */
object PairingReset {
    private const val TAG = "PassporterShell"

    /** Drops it all; failures are logged. */
    fun run(context: Context) {
        val app = context.applicationContext
        step("the queued answers") {
            val work = WorkManager.getInstance(app)
            listOf(Outbox.TAG, CarOutbox.TAG).map { work.cancelAllWorkByTag(it).result }.forEach { it.get() }
        }
        step("the frozen bodies") { PreferenceFrozenBodies(app).clear() }
        step("the agents' notifications") { cancelNotices(app) }
        step("the dictation takes") { Recorder.discardAll(app) }
    }

    /** Cancels every agent's notification ([NoticeNotification.ID]), under [NotificationLock]. */
    private fun cancelNotices(context: Context) = synchronized(NotificationLock) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.activeNotifications
            .filter { it.id == NoticeNotification.ID }
            .forEach { manager.cancel(it.tag, it.id) }
    }

    private inline fun step(what: String, work: () -> Unit) {
        try {
            work()
        } catch (e: Exception) {
            Log.e(TAG, "could not drop $what", e)
        }
    }
}
