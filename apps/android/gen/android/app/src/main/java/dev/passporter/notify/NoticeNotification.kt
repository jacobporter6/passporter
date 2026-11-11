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
import dev.passporter.notify.actions.NoticeActions

/**
 * Builds the notification of one [Notice]: a `MessagingStyle` conversation
 * per agent, grouped by the agent's conversation shortcut and `LocusId`
 * (design §6.6).
 *
 * Car-safe text: the conversation shows the agent's name, then its one
 * line (what it asks, or the turn's headline) and the *Decide* line, and
 * nothing else. The actions (reply, *Allow once* / *Deny*) are
 * [NoticeActions]'s.
 */
object NoticeNotification {
    /** Channel of blocked agents: high importance, it waits for you. */
    const val CHANNEL_NEEDS_YOU = "needs_you"

    /** Channel of finished turns: default importance. */
    const val CHANNEL_FINISHED = "finished"

    /**
     * The extra naming the question a notification asks: the dialog's
     * pending id, or [TURN] for a finished turn ([question]). An answer
     * from the shade checks it, so a tap on an older notice never lands on
     * newer news.
     */
    const val EXTRA_QUESTION = "dev.passporter.notify.QUESTION"

    /** [EXTRA_QUESTION] of a finished turn. */
    const val TURN = "turn"

    /** The notification id; the tag ([tag]) tells agents apart. */
    const val ID = 1

    /** Intent action that opens the app at one agent ([openAgent]). */
    const val ACTION_OPEN_AGENT = "dev.passporter.notify.OPEN_AGENT"

    /** The scheme and host of [openAgent]'s data: `passporter://agent/<id>`. */
    private const val SCHEME = "passporter"
    private const val HOST = "agent"

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

    /**
     * The notification of [notice]. It is news ([NotificationPlanner]), so
     * it alerts even when it replaces the agent's last one. Its actions are
     * [NoticeActions]'s: what the notice can be answered with from the shade.
     */
    fun build(context: Context, notice: Notice, now: Long): Notification {
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
        val builder = conversation(context, notice.agentId, notice.state, style, now)
            .setContentTitle(notice.name)
            .setContentText(notice.decide?.let { context.getString(R.string.notify_decide, it) } ?: headline)
            // News that replaces the agent's last notification sounds again.
            .setOnlyAlertOnce(false)
        NoticeActions.build(context, notice).forEach(builder::addAction)
        question(notice)?.let { builder.addExtras(Bundle().apply { putString(EXTRA_QUESTION, it) }) }
        return builder.build()
    }

    /**
     * What every notification of [agent]'s conversation shares: the
     * channel of [state], [style], the shortcut and `LocusId`, the tap that opens the
     * agent, and the private lock-screen version. [build] adds the news;
     * [dev.passporter.notify.actions.ActionFeedback] the progress of an
     * answer given from the shade.
     */
    internal fun conversation(
        context: Context,
        agent: AgentId,
        state: NoticeState,
        style: NotificationCompat.MessagingStyle,
        now: Long,
    ): NotificationCompat.Builder {
        val shortcutId = shortcutId(agent)
        val tap = PendingIntent.getActivity(
            context,
            0,
            openAgent(context, agent),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(context, channel(state))
            .setSmallIcon(R.drawable.ic_notification)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setStyle(style)
            // The lock screen shows that an agent has news, never what it says.
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setPublicVersion(publicVersion(context, state))
            .setShortcutId(shortcutId)
            .setLocusId(LocusIdCompat(shortcutId))
            .setContentIntent(tap)
            .setAutoCancel(true)
            .setWhen(now)
            .setShowWhen(true)
            // The text is agent output: no system-suggested actions from it.
            .setAllowSystemGeneratedContextualActions(false)
    }

    /**
     * Opens the app at [agent]: `passporter://agent/<id>` (the id as one
     * percent-encoded path segment; read it back with [agentToOpen]) with
     * action [ACTION_OPEN_AGENT], explicit to [MainActivity]. The data keeps
     * each agent's pending intent apart.
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

    /** What a locked phone shows instead: no agent text at all. */
    private fun publicVersion(context: Context, state: NoticeState): Notification =
        NotificationCompat.Builder(context, channel(state))
            .setSmallIcon(R.drawable.ic_notification)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setContentTitle(context.getString(R.string.app_name))
            .setContentText(
                context.getString(
                    when (state) {
                        NoticeState.BLOCKED -> R.string.notify_public_blocked
                        else -> R.string.notify_public_finished
                    },
                ),
            )
            .build()

    /** What [notice] asks, as [EXTRA_QUESTION]; `null` when it cannot be answered from the shade. */
    fun question(notice: Notice): String? =
        when (notice.state) {
            NoticeState.BLOCKED -> notice.pendingId?.value
            NoticeState.FINISHED -> TURN
            NoticeState.UNKNOWN -> null
        }

    /** The state whose channel is [channel], or `null` for any other channel. */
    internal fun stateOf(channel: String?): NoticeState? =
        when (channel) {
            CHANNEL_NEEDS_YOU -> NoticeState.BLOCKED
            CHANNEL_FINISHED -> NoticeState.FINISHED
            else -> null
        }

    /** The channel of a notice in [state]. */
    private fun channel(state: NoticeState): String =
        when (state) {
            NoticeState.BLOCKED -> CHANNEL_NEEDS_YOU
            else -> CHANNEL_FINISHED
        }
}
