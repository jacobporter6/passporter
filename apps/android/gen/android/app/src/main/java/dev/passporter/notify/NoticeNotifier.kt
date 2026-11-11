package dev.passporter.notify

import android.Manifest
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.content.pm.ShortcutManagerCompat

/**
 * Posts the notifications of decrypted push messages: the one entry point
 * of this package for the push receiver.
 *
 * What is shown now is read back from the shade (each notification keeps
 * its notice in its extras), so nothing is stored and a dismissed
 * notification counts as not shown.
 */
class NoticeNotifier(context: Context) {
    private val context = context.applicationContext
    private val manager = NotificationManagerCompat.from(this.context)

    /**
     * Shows the notices in [payload], the decrypted plaintext of a push
     * message. Returns the parse, so the caller can fall back (for example
     * to a generic notification) on [NoticeParse.Invalid]; the reason is
     * logged by field name only.
     */
    fun show(payload: String): NoticeParse {
        val parse = NoticeParser.parse(payload)
        when (parse) {
            is NoticeParse.Parsed -> show(parse.message)
            is NoticeParse.Invalid -> Log.w(TAG, "invalid notice message: ${parse.error}")
        }
        return parse
    }

    /** Shows [message]'s news: see [NotificationPlanner] for what alerts. */
    fun show(message: NoticeMessage) {
        if (!canPost()) return
        NoticeNotification.createChannels(context)
        val now = System.currentTimeMillis()
        for (posting in NotificationPlanner.plan(message, shown())) {
            val notice = posting.notice
            ShortcutManagerCompat.pushDynamicShortcut(
                context,
                NoticeNotification.shortcut(context, notice),
            )
            val notification = NoticeNotification.build(context, notice, posting.alert, now)
            try {
                manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, notification)
            } catch (_: SecurityException) {
                // The permission was revoked between the check and the post.
                return
            }
        }
    }

    /** The notice each of our notifications in the shade shows, per agent. */
    private fun shown(): Map<AgentId, Notice> =
        context.getSystemService(NotificationManager::class.java)
            .activeNotifications
            .filter { it.id == NoticeNotification.ID }
            .mapNotNull { NoticeNotification.shownNotice(it.notification.extras) }
            .associateBy { it.agentId }

    private fun canPost(): Boolean {
        val granted = Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        return granted && manager.areNotificationsEnabled()
    }

    private companion object {
        const val TAG = "PassporterNotify"
    }
}
