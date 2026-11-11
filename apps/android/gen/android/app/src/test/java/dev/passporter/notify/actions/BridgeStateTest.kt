package dev.passporter.notify.actions

import dev.passporter.notify.PendingId
import dev.passporter.notify.agent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class BridgeStateTest {
    @Test
    fun agentsAreReadTolerantly() {
        val body = """[
            {"id":"w1:p1","status":"done","seq":7,"statusSince":1000,"unread":true,"kind":"claude","newer":1},
            {"id":"w1/p2","status":"idle","seq":1,"statusSince":0},
            {"id":"w1:p3","status":"dreaming","seq":2,"statusSince":5},
            {"id":"w1:p4","status":"idle"},
            "noise"]"""
        assertEquals(
            listOf(
                AgentState(agent("w1:p1"), AgentStatus.DONE, 7, 1000, unread = true),
                AgentState(agent("w1:p3"), AgentStatus.UNKNOWN, 2, 5, unread = false),
            ),
            BridgeState.agents(body),
        )
        assertNull(BridgeState.agents("{}"))
        assertNull(BridgeState.agents("nope"))
    }

    @Test
    fun aDialogIsReadWithItsOptions() {
        val open = BridgeState.dialog(PERMISSION) as BridgeState.Dialog.Open
        assertEquals(requireNotNull(PendingId.parse("screen-2-ab")), open.state.id)
        assertEquals(
            listOf(
                DialogOption("yes", lowRisk = true, acceptsFeedback = false),
                DialogOption("yes_always", lowRisk = false, acceptsFeedback = false),
                DialogOption("no_feedback", lowRisk = true, acceptsFeedback = true),
            ),
            open.state.options,
        )
        assertNull(open.state.lowRisk("yes_always"))
        assertEquals(BridgeState.Dialog.None, BridgeState.dialog(" null\n"))
        assertNull(BridgeState.dialog("""{"kind":"permission"}"""))
        assertNull(BridgeState.dialog("[]"))
    }
}
