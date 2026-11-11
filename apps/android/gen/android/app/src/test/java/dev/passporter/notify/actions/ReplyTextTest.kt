package dev.passporter.notify.actions

import org.junit.Assert.assertEquals
import org.junit.Test

class ReplyTextTest {
    private fun clean(raw: String?, action: NoticeAction = NoticeAction.REPLY) = ReplyText.clean(raw, action)

    @Test
    fun aPromptKeepsItsLineBreaksANoteGetsSpaces() {
        assertEquals(ReplyText.Result.Clean("run it\nthen merge"), clean(" run it\r\nthen merge\u0007 "))
        assertEquals(ReplyText.Result.Clean("a b c"), clean("a\tb\nc", NoticeAction.DENY_WITH_NOTE))
    }

    @Test
    fun emptyAndTooLongTextIsNotSent() {
        assertEquals(ReplyText.Result.Problem(Unsendable.EMPTY), clean(null))
        assertEquals(ReplyText.Result.Problem(Unsendable.EMPTY), clean(" \n\t "))
        assertEquals(ReplyText.Result.Problem(Unsendable.EMPTY), clean("\u200d\ufe0f\u200c"))
        assertEquals(ReplyText.Result.Clean("a\nb"), clean("a\u2028b"))
        assertEquals(ReplyText.Result.Clean("abc"), clean("a\u200fb\u200bc\u202e"))
        assertEquals(ReplyText.Result.Clean("a b"), clean("a\u2029b", NoticeAction.DENY_WITH_NOTE))
        assertEquals(ReplyText.Result.Clean("é".repeat(ReplyText.MAX_BYTES / 2)), clean("é".repeat(ReplyText.MAX_BYTES / 2)))
        assertEquals(ReplyText.Result.Problem(Unsendable.TOO_LONG), clean("é".repeat(ReplyText.MAX_BYTES / 2 + 1)))
    }
}
