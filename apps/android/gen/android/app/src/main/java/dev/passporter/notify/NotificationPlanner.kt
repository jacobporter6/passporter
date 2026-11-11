package dev.passporter.notify

/** One notification to post (or replace) for [notice]'s agent. */
data class Posting(
    /** What to show. */
    val notice: Notice,
    /** Whether it may sound and buzz: only for news. */
    val alert: Boolean,
)

/**
 * Decides what a [NoticeMessage] changes in the shade, "only news
 * disrupts" (design §6.6). Pure, so it is tested without Android.
 *
 * A message is a delta ([NoticeMessage]): an agent it does not mention is
 * left as it is. For each notice:
 *
 * - an [NoticeState.UNKNOWN] state is skipped: this app cannot say what it
 *   means, and what is shown stays;
 * - news ([Notice.sameNewsAs] is false, or nothing is shown for the agent)
 *   is posted with an alert;
 * - the same news with another name or car flag replaces the notification
 *   silently;
 * - the same notice again (a re-sent message) changes nothing.
 *
 * Postings come oldest first, as the message lists them, one per agent
 * (the last notice of an agent wins, as in the bridge's batch).
 */
object NotificationPlanner {
    /** What to post for [message], given the notices [shown] per agent now. */
    fun plan(message: NoticeMessage, shown: Map<AgentId, Notice>): List<Posting> =
        message.notices
            .filter { it.state != NoticeState.UNKNOWN }
            .associateBy { it.agentId }
            .values
            .mapNotNull { notice ->
                val current = shown[notice.agentId]
                when {
                    current == null || !notice.sameNewsAs(current) -> Posting(notice, alert = true)
                    notice == current -> null
                    else -> Posting(notice, alert = false)
                }
            }
}
