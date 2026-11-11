package dev.passporter.notify

/**
 * Decides what a [NoticeMessage] posts, "only news disrupts" (design
 * §6.6). Pure, so it is tested without Android.
 *
 * The bridge sends a notice only for a new event (a new dialog, a finished
 * turn, its *Decide* line), and only for a change since its last message.
 * So every notice of a new message is news, even one that reads like the
 * notification it replaces (a second dialog it could not parse, a second
 * turn without a headline). The one repeat is the same message delivered
 * again, which its `ping` gives away; [NoticeNotifier] drops it before it
 * gets here ([RecentPings]), so it never buzzes.
 *
 * A message is a delta ([NoticeMessage]): an agent it does not mention
 * keeps whatever it shows.
 */
object NotificationPlanner {
    /**
     * The notices to post for [message], oldest first and one per agent
     * (its last notice in the message, as in the bridge's batch). An
     * agent whose last notice has an [NoticeState.UNKNOWN] state is
     * skipped: this app cannot say what it means, so what is shown for
     * that agent stays.
     */
    fun plan(message: NoticeMessage): List<Notice> {
        val latest = LinkedHashMap<AgentId, Notice>()
        for (notice in message.notices) {
            latest.remove(notice.agentId)
            latest[notice.agentId] = notice
        }
        return latest.values.filter { it.state != NoticeState.UNKNOWN }
    }
}
