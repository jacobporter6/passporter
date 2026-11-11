package dev.passporter.notify

import android.app.Notification
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import androidx.core.content.LocusIdCompat
import androidx.core.content.pm.ShortcutInfoCompat
import dev.passporter.app.MainActivity
import dev.passporter.app.R

/**
 * Builds the notification of one [Notice]: a `MessagingStyle` conversation
 * per agent, grouped by the agent's conversation shortcut and `LocusId`
 * (design §6.6).
 *
 * Car-safe text: the conversation shows the agent's name, then its one
 * line (what it asks, or the turn's headline) and the *Decide* line, and
 * nothing else. Actions (reply, *Allow once* / *Deny*, mark as read) are a
 * later task's; they belong in [build].
 */
object NoticeNotification {
    /** Channel of blocked agents: high importance, it waits for you. */
    const val CHANNEL_NEEDS_YOU = "needs_you"

    /** Channel of finished turns: default importance. */
    const val CHANNEL_FINISHED = "finished"

    /** The notification id; the tag ([tag]) tells agents apart. */
    const val ID = 1

    /** Intent action that opens the app at one agent ([openAgent]). */
    const val ACTION_OPEN_AGENT = "dev.passporter.notify.OPEN_AGENT"

    /** The scheme and host of [openAgent]'s data: `passporter://agent/<id>`. */
    private const val SCHEME = "passporter"
    private const val HOST = "agent"

    // The shown notice, so the next message can tell news from a repeat.
    private const val EXTRA_AGENT_ID = "dev.passporter.notify.agentId"
    private const val EXTRA_NAME = "dev.passporter.notify.name"
    private const val EXTRA_STATE = "dev.passporter.notify.state"
    private const val EXTRA_LINE = "dev.passporter.notify.line"
    private const val EXTRA_DECIDE = "dev.passporter.notify.decide"
    private const val EXTRA_PENDING_ID = "dev.passporter.notify.pendingId"
    private const val EXTRA_ANNOUNCE = "dev.passporter.notify.announce"

    /** The notification tag of [agent]'s conversation. */
    fun tag(agent: AgentId): String = agent.value

    /** The conversation shortcut (and `LocusId`) of [agent]. */
    fun shortcutId(agent: AgentId): String = "agent:${agent.value}"

    /** Creates (or updates the names of) the channels; cheap to repeat. */
    fun createChannels(context: Context) {
        val channels = listOf(
            NotificationChannelCompat.Builder(
                CHANNEL_NEEDS_YOU,
                NotificationManagerCompat.IMPORTANCE_HIGH,
            ).setName(context.getString(R.string.notify_channel_needs_you))
                .setDescription(context.getString(R.string.notify_channel_needs_you_description))
                .build(),
            NotificationChannelCompat.Builder(
                CHANNEL_FINISHED,
                NotificationManagerCompat.IMPORTANCE_DEFAULT,
            ).setName(context.getString(R.string.notify_channel_finished))
                .setDescription(context.getString(R.string.notify_channel_finished_description))
                .build(),
        )
        NotificationManagerCompat.from(context).createNotificationChannelsCompat(channels)
    }

    /** The agent as the conversation's other person. */
    fun person(notice: Notice): Person =
        Person.Builder()
            .setName(notice.name)
            .setKey(notice.agentId.value)
            .setBot(true)
            .build()

    /** The long-lived conversation shortcut the notification belongs to. */
    fun shortcut(context: Context, notice: Notice): ShortcutInfoCompat {
        val id = shortcutId(notice.agentId)
        return ShortcutInfoCompat.Builder(context, id)
            .setShortLabel(notice.name)
            .setLongLived(true)
            .setPerson(person(notice))
            .setLocusId(LocusIdCompat(id))
            .setIntent(openAgent(context, notice.agentId))
            .build()
    }

    /** The notification of [notice]; [alert] says whether it may sound. */
    fun build(context: Context, notice: Notice, alert: Boolean, now: Long): Notification {
        val agent = person(notice)
        val style = NotificationCompat.MessagingStyle(
            Person.Builder().setName(context.getString(R.string.notify_you)).build(),
        )
        val headline = notice.line ?: context.getString(
            when (notice.state) {
                NoticeState.BLOCKED -> R.string.notify_blocked_fallback
                else -> R.string.notify_finished_fallback
            },
        )
        style.addMessage(headline, now, agent)
        notice.decide?.let { style.addMessage(context.getString(R.string.notify_decide, it), now, agent) }
        val shortcutId = shortcutId(notice.agentId)
        val tap = PendingIntent.getActivity(
            context,
            0,
            openAgent(context, notice.agentId),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(context, channel(notice.state))
            .setSmallIcon(R.drawable.ic_notification)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setStyle(style)
            .setContentTitle(notice.name)
            .setContentText(notice.decide ?: headline)
            .setShortcutId(shortcutId)
            .setLocusId(LocusIdCompat(shortcutId))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .setWhen(now)
            .setShowWhen(true)
            // A quiet update of a showing notification never sounds; news
            // replacing one sounds again.
            .setOnlyAlertOnce(!alert)
            // The text is agent output: no system-suggested actions from it.
            .setAllowSystemGeneratedContextualActions(false)
            .addExtras(extras(notice))
            .build()
    }

    /** The notice a posted notification of ours shows, from its extras. */
    fun shownNotice(extras: Bundle): Notice? {
        val agentId = extras.getString(EXTRA_AGENT_ID)?.let(AgentId::parse) ?: return null
        val name = extras.getString(EXTRA_NAME) ?: return null
        val state = extras.getString(EXTRA_STATE)?.let(NoticeState::fromWire) ?: return null
        return Notice(
            agentId = agentId,
            name = name,
            state = state,
            line = extras.getString(EXTRA_LINE),
            decide = extras.getString(EXTRA_DECIDE),
            pendingId = extras.getString(EXTRA_PENDING_ID)?.let(PendingId::parse),
            announce = extras.getBoolean(EXTRA_ANNOUNCE),
        )
    }

    /**
     * Opens the app at [agent]: `passporter://agent/<id>` with action
     * [ACTION_OPEN_AGENT], explicit to [MainActivity]. The data keeps each
     * agent's pending intent apart.
     */
    fun openAgent(context: Context, agent: AgentId): Intent =
        Intent(context, MainActivity::class.java)
            .setAction(ACTION_OPEN_AGENT)
            .setData(Uri.Builder().scheme(SCHEME).authority(HOST).appendPath(agent.value).build())

    /** The agent an [openAgent] intent names, or `null` for any other intent. */
    fun agentToOpen(intent: Intent): AgentId? {
        val data = intent.data ?: return null
        val matches = intent.action == ACTION_OPEN_AGENT && data.scheme == SCHEME && data.host == HOST
        return data.pathSegments.singleOrNull()?.takeIf { matches }?.let(AgentId::parse)
    }

    private fun channel(state: NoticeState): String =
        when (state) {
            NoticeState.BLOCKED -> CHANNEL_NEEDS_YOU
            else -> CHANNEL_FINISHED
        }

    private fun extras(notice: Notice): Bundle =
        Bundle().apply {
            putString(EXTRA_AGENT_ID, notice.agentId.value)
            putString(EXTRA_NAME, notice.name)
            putString(EXTRA_STATE, notice.state.wire)
            putString(EXTRA_LINE, notice.line)
            putString(EXTRA_DECIDE, notice.decide)
            putString(EXTRA_PENDING_ID, notice.pendingId?.value)
            putBoolean(EXTRA_ANNOUNCE, notice.announce)
        }
}
