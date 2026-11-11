package dev.passporter.notify

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.content.pm.ShortcutManagerCompat

/** What [NoticeNotifier.show] did with a payload. */
sealed interface NoticeOutcome {
    /** Posted [count] notifications (none for a bare ping). */
    data class Posted(
        /** How many. */
        val count: Int,
    ) : NoticeOutcome

    /** The same message was shown before: nothing posted. */
    data object Duplicate : NoticeOutcome

    /** Notifications are off for the app (or not yet allowed): nothing posted. */
    data object NotAllowed : NoticeOutcome

    /** Not a notice message ([NoticeParser]): nothing posted. */
    data class Invalid(
        /** What was wrong. */
        val error: NoticeError,
    ) : NoticeOutcome
}

/**
 * Posts the notifications of decrypted push messages: the one entry point
 * of this package for the push receiver.
 *
 * Every notice of a new message is news and alerts ([NotificationPlanner]);
 * a message delivered twice is recognised by its ping ([RecentPings]).
 */
class NoticeNotifier(context: Context) {
    private val context = context.applicationContext
    private val manager = NotificationManagerCompat.from(this.context)
    private val pings = RecentPings(this.context)

    /**
     * Shows the notices in [payload], the decrypted plaintext of a push
     * message. The outcome lets the caller fall back (for example to a
     * generic notification); an invalid payload is logged by field name
     * only.
     */
    fun show(payload: String): NoticeOutcome =
        when (val parse = NoticeParser.parse(payload)) {
            is NoticeParse.Parsed -> show(parse.message)
            is NoticeParse.Invalid -> {
                Log.w(TAG, "invalid notice message: ${parse.error}")
                NoticeOutcome.Invalid(parse.error)
            }
        }

    /** Shows [message]'s news. */
    fun show(message: NoticeMessage): NoticeOutcome {
        if (!canPost()) return NoticeOutcome.NotAllowed
        // One message at a time, so a retry racing its original is seen;
        // and never in the middle of an answer's update of the same
        // conversation (dev.passporter.notify.actions.ActionFeedback).
        synchronized(NotificationLock) {
            if (message.ping in pings.all()) return NoticeOutcome.Duplicate
            pings.add(message.ping)
            val notices = NotificationPlanner.plan(message)
            NoticeNotification.createChannels(context)
            val now = System.currentTimeMillis()
            for ((posted, notice) in notices.withIndex()) {
                ShortcutManagerCompat.pushDynamicShortcut(
                    context,
                    NoticeNotification.shortcut(context, notice),
                )
                val notification = NoticeNotification.build(context, notice, now)
                try {
                    manager.notify(NoticeNotification.tag(notice.agentId), NoticeNotification.ID, notification)
                } catch (_: SecurityException) {
                    // The permission was revoked between the check and the post.
                    return if (posted == 0) NoticeOutcome.NotAllowed else NoticeOutcome.Posted(posted)
                }
            }
            return NoticeOutcome.Posted(notices.size)
        }
    }

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
