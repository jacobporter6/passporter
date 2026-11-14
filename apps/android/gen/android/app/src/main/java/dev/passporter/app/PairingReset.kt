package dev.passporter.app

import android.app.NotificationManager
import android.content.Context
import android.util.Log
import androidx.core.content.pm.ShortcutManagerCompat
import androidx.work.WorkManager
import dev.passporter.car.CarOutbox
import dev.passporter.device.DeviceAuth
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.NotificationLock
import dev.passporter.notify.actions.Outbox
import dev.passporter.notify.actions.PreferenceFrozenBodies
import dev.passporter.push.BuiltInPush
import dev.passporter.push.NativePushService
import dev.passporter.push.NativeStatus
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
 * - the agents' conversation shortcuts, dynamic and long-lived (the share
 *   sheet and the launcher keep the old agents' names, which are agent
 *   output, and a tap on one would open the new bridge's agent of the same
 *   id); one pinned to the home screen cannot be removed, so it is
 *   disabled; the launcher's static *Bridge* shortcut stays;
 * - the dictation takes nobody has released ([Recorder.discardAll]),
 *   which the new bridge's PWA would adopt;
 * - the built-in transport's stream, registration and keys
 *   ([BuiltInPush]): the next registration is the new pairing's, with new
 *   keys, so what the old bridge still sends cannot be decrypted.
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
        // First, so no notice of the old pairing is shown after the shade is cleared.
        step("the built-in transport") {
            val receiver = BuiltInPush.receiver(app)
            val paired = DeviceAuth.get(app).savedDevice()?.value
            // Unless it is already the new pairing's (a refresh that ran first:
            // its reset already ended the old stream, and the service reconnects).
            // Not paired (a forget) always resets, even with no owner: no state stands then.
            if (paired == null || receiver.record().owner != paired) {
                NativePushService.stop(app)
                receiver.reset()
                NativeStatus.reset()
            } else {
                // The new pairing's state stands; only a clock verdict from before goes.
                NativeStatus.clockOff = false
            }
        }
        step("the queued answers") {
            val work = WorkManager.getInstance(app)
            listOf(Outbox.TAG, CarOutbox.TAG).map { work.cancelAllWorkByTag(it).result }.forEach { it.get() }
        }
        step("the frozen bodies") { PreferenceFrozenBodies(app).clear() }
        // One hold of the lock, so no notice is posted between the two.
        synchronized(NotificationLock) {
            step("the agents' notifications") { cancelNotices(app) }
            step("the agents' shortcuts") { removeShortcuts(app) }
        }
        step("the dictation takes") { Recorder.discardAll(app) }
    }

    /** Cancels every agent's notification ([NoticeNotification.ID]), under [NotificationLock]. */
    private fun cancelNotices(context: Context) = synchronized(NotificationLock) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.activeNotifications
            .filter { it.id == NoticeNotification.ID }
            .forEach { manager.cancel(it.tag, it.id) }
    }

    /**
     * Removes every agent's conversation shortcut ([NoticeNotification.isShortcutId]),
     * cached long-lived ones included, and disables pinned ones, under
     * [NotificationLock] (which [dev.passporter.notify.NoticeNotifier]
     * holds while it pushes them; pushing one anew enables it again).
     */
    private fun removeShortcuts(context: Context) = synchronized(NotificationLock) {
        val flags = ShortcutManagerCompat.FLAG_MATCH_DYNAMIC or
            ShortcutManagerCompat.FLAG_MATCH_CACHED or
            ShortcutManagerCompat.FLAG_MATCH_PINNED
        val ids = ShortcutManagerCompat.getShortcuts(context, flags)
            .map { it.id }
            .filter(NoticeNotification::isShortcutId)
            .distinct()
        if (ids.isEmpty()) return@synchronized
        ShortcutManagerCompat.removeDynamicShortcuts(context, ids)
        // The system's cached copies, kept for the conversations' notifications.
        ShortcutManagerCompat.removeLongLivedShortcuts(context, ids)
        ShortcutManagerCompat.disableShortcuts(context, ids, context.getString(R.string.shortcut_agent_disabled))
    }

    private inline fun step(what: String, work: () -> Unit) {
        try {
            work()
        } catch (e: Exception) {
            Log.e(TAG, "could not drop $what", e)
        }
    }
}
