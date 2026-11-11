package dev.passporter.notify

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Robolectric for the platform's `org.json`, which the JVM stubs lack. */
@RunWith(RobolectricTestRunner::class)
class NoticeParserTest {
    private fun parsed(payload: String): NoticeMessage {
        val parse = NoticeParser.parse(payload)
        check(parse is NoticeParse.Parsed) { "not parsed: $parse" }
        return parse.message
    }

    private fun error(payload: String): NoticeError {
        val parse = NoticeParser.parse(payload)
        check(parse is NoticeParse.Invalid) { "parsed: $parse" }
        return parse.error
    }

    @Test
    fun theBridgesMessagesParse() {
        // As the bridge serializes them: absent options left out.
        val message = parsed(
            """{"ping":"0123456789abcdef0123456789abcdef","notices":[
              {"agentId":"w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4","name":"widget",
               "state":"blocked","line":"Run `rm -rf build`?","pendingId":"screen-2-ab",
               "announce":true},
              {"agentId":"w1:p2","name":"docs","state":"finished",
               "line":"Fixed the flaky test","decide":"Merge PR #12?"}]}""",
        )
        assertEquals("0123456789abcdef0123456789abcdef", message.ping)
        assertEquals(
            listOf(
                notice(
                    agent = "w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4",
                    line = "Run `rm -rf build`?",
                ),
                notice(
                    agent = "w1:p2",
                    name = "docs",
                    state = NoticeState.FINISHED,
                    line = "Fixed the flaky test",
                    decide = "Merge PR #12?",
                    pending = null,
                    announce = false,
                ),
            ),
            message.notices,
        )
    }

    @Test
    fun aBarePingHasNoNotices() {
        assertEquals(NoticeMessage("ab", emptyList()), parsed("""{"ping":"ab"}"""))
        assertEquals(NoticeMessage("ab", emptyList()), parsed("""{"ping":"ab","notices":null}"""))
    }

    @Test
    fun whatANewerBridgeAddsIsTolerated() {
        // The bridge's own test of the same shape (protocol::push).
        val message = parsed(
            """{"ping":"ab","extra":1,"notices":[{"agentId":"w1:p1","name":"n",
                "state":"thinking","announce":true,"mood":"?","line":null}]}""",
        )
        val notice = message.notices.single()
        assertEquals(NoticeState.UNKNOWN, notice.state)
        assertEquals(true, notice.announce)
        assertNull(notice.line)
    }

    @Test
    fun malformedInputIsATypedErrorNeverACrash() {
        val cases = mapOf(
            "" to NoticeError.NotAnObject,
            "not json" to NoticeError.NotAnObject,
            "[]" to NoticeError.NotAnObject,
            "{}" to NoticeError.Missing("ping"),
            """{"ping":7}""" to NoticeError.WrongType("ping"),
            """{"ping":"ab","notices":{}}""" to NoticeError.WrongType("notices"),
            """{"ping":"ab","notices":[1]}""" to NoticeError.WrongType("notices[0]"),
            """{"ping":"ab","notices":[{"name":"n","state":"blocked"}]}""" to
                NoticeError.Missing("notices[0].agentId"),
            """{"ping":"ab","notices":[{"agentId":"w1/p1","name":"n","state":"blocked"}]}""" to
                NoticeError.InvalidId("notices[0].agentId"),
            """{"ping":"ab","notices":[{"agentId":"a~b~c","name":"n","state":"blocked"}]}""" to
                NoticeError.InvalidId("notices[0].agentId"),
            """{"ping":"ab","notices":[{"agentId":"w1:p1","name":"n","state":"blocked",
                "pendingId":".."}]}""" to NoticeError.InvalidId("notices[0].pendingId"),
            """{"ping":"ab","notices":[{"agentId":"w1:p1","state":"blocked"}]}""" to
                NoticeError.Missing("notices[0].name"),
            """{"ping":"ab","notices":[{"agentId":"w1:p1","name":"n","state":1}]}""" to
                NoticeError.WrongType("notices[0].state"),
            """{"ping":"ab","notices":[{"agentId":"w1:p1","name":"n","state":"blocked",
                "announce":"yes"}]}""" to NoticeError.WrongType("notices[0].announce"),
            """{"ping":"ab","notices":[{"agentId":"w1:p1","name":"n","state":"finished",
                "decide":["x"]}]}""" to NoticeError.WrongType("notices[0].decide"),
        )
        for ((payload, expected) in cases) {
            assertEquals(payload, expected, error(payload))
        }
    }

    @Test
    fun textsAreCleanedAndBoundedAgain() {
        val long = "x".repeat(500)
        val notice = parsed(
            """{"ping":"ab","notices":[{"agentId":"w1:p1","name":"​\n",
                "state":"finished","line":"a\nb‮c","decide":"$long"}]}""",
        ).notices.single()
        assertEquals("Agent", notice.name)
        assertEquals("a bc", notice.line)
        assertEquals(Notice.MAX_LINE_CHARS, notice.decide?.length)
    }

    @Test
    fun onlyTheNewestNoticesAreKept() {
        val notices = (1..6).joinToString(",") {
            """{"agentId":"w1:p$it","name":"n","state":"blocked"}"""
        }
        val kept = parsed("""{"ping":"ab","notices":[$notices]}""").notices.map { it.agentId.value }
        assertEquals(listOf("w1:p3", "w1:p4", "w1:p5", "w1:p6"), kept)
    }

    @Test
    fun idsFollowTheBridgesCharset() {
        for (valid in listOf("w6:p1", "w6:p1~abc", "a.b_c-d", "x".repeat(128))) {
            assertEquals(valid, AgentId.parse(valid)?.value)
        }
        for (invalid in listOf("", "~abc", "w6:p1~", "..", "w6 p1", "é", "x".repeat(129))) {
            assertNull(invalid, AgentId.parse(invalid))
        }
        assertNull(PendingId.parse("a~b"))
    }
}
