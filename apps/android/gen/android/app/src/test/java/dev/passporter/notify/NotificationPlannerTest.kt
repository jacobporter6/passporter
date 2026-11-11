package dev.passporter.notify

import org.junit.Assert.assertEquals
import org.junit.Test

class NotificationPlannerTest {
    @Test
    fun everyNoticeOfAMessageIsPosted() {
        val blocked = notice()
        val finished = notice(agent = "w1:p2", state = NoticeState.FINISHED, pending = null)
        assertEquals(listOf(blocked, finished), NotificationPlanner.plan(messageOf(blocked, finished)))
        assertEquals(emptyList<Notice>(), NotificationPlanner.plan(messageOf()))
    }

    @Test
    fun eachAgentPostsItsLastNoticeInItsLastPlace() {
        val first = notice(line = "first")
        val other = notice(agent = "w1:p2")
        val last = notice(line = "last")
        assertEquals(listOf(other, last), NotificationPlanner.plan(messageOf(first, other, last)))
    }

    @Test
    fun anAgentWhoseLastStateIsUnknownIsLeftAlone() {
        val known = notice()
        val unknown = notice(state = NoticeState.UNKNOWN)
        val other = notice(agent = "w1:p2")
        assertEquals(listOf(other), NotificationPlanner.plan(messageOf(known, other, unknown)))
        assertEquals(listOf(known), NotificationPlanner.plan(messageOf(unknown, known)))
    }
}
