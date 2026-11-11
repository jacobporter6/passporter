package dev.passporter.notify

import org.junit.Assert.assertEquals
import org.junit.Test

class NotificationPlannerTest {
    private fun plan(message: NoticeMessage, vararg shown: Notice) =
        NotificationPlanner.plan(message, shown.associateBy { it.agentId })

    @Test
    fun newsAlerts() {
        val blocked = notice()
        assertEquals(listOf(Posting(blocked, alert = true)), plan(messageOf(blocked)))
    }

    @Test
    fun aResentUnchangedNoticeChangesNothing() {
        val blocked = notice()
        assertEquals(emptyList<Posting>(), plan(messageOf(blocked), blocked))
    }

    @Test
    fun aNewDialogOrStateOrDecideLineIsNewsAgain() {
        val shown = notice()
        val newDialog = notice(pending = "screen-3-cd")
        val newAsk = notice(line = "Run `pnpm build`?")
        val finished = notice(state = NoticeState.FINISHED, line = "Done", pending = null)
        val decided = finished.copy(decide = "Merge PR #12?")
        for (news in listOf(newDialog, newAsk, finished)) {
            assertEquals(listOf(Posting(news, alert = true)), plan(messageOf(news), shown))
        }
        assertEquals(listOf(Posting(decided, alert = true)), plan(messageOf(decided), finished))
    }

    @Test
    fun theSameNewsUnderANewNameIsUpdatedSilently() {
        val shown = notice()
        for (renamed in listOf(notice(name = "widget 2"), notice(announce = false))) {
            assertEquals(listOf(Posting(renamed, alert = false)), plan(messageOf(renamed), shown))
        }
    }

    @Test
    fun agentsTheMessageDoesNotMentionAreLeftAlone() {
        // A message is a delta: w1:p1 is still blocked, it just has no news.
        val other = notice(agent = "w1:p2", state = NoticeState.FINISHED, pending = null)
        assertEquals(
            listOf(Posting(other, alert = true)),
            plan(messageOf(other), notice()),
        )
    }

    @Test
    fun unknownStatesAreSkippedAndEachAgentPostsOnce() {
        val unknown = notice(agent = "w1:p3", state = NoticeState.UNKNOWN)
        val first = notice(line = "first")
        val last = notice(line = "last")
        val other = notice(agent = "w1:p2")
        assertEquals(
            listOf(Posting(last, alert = true), Posting(other, alert = true)),
            plan(messageOf(first, unknown, other, last)),
        )
        assertEquals(emptyList<Posting>(), plan(NoticeMessage("ab", emptyList())))
    }
}
