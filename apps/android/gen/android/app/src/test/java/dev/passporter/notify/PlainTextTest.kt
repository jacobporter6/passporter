package dev.passporter.notify

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PlainTextTest {
    @Test
    fun textBecomesOneCleanLine() {
        assertEquals(
            "Run `ls` now[2J",
            PlainText.oneLine("  Run\n`ls`‮  now\u001b[2J ", 60),
        )
        assertEquals("a b", PlainText.oneLine("a  b", 60))
        assertNull(PlainText.oneLine("​ \n\t", 10))
    }

    @Test
    fun longTextIsCutWithAnEllipsisInsideTheBound() {
        val cut = requireNotNull(PlainText.oneLine("word ".repeat(100), 20))
        assertEquals(20, cut.codePointCount(0, cut.length))
        assertEquals("word word word word…", cut)
        assertEquals("short", PlainText.oneLine("short", 5))
    }

    @Test
    fun boundsCountCharactersNotUtf16Units() {
        val emoji = "😀"
        assertEquals(emoji.repeat(3), PlainText.oneLine(emoji.repeat(3), 3))
        val cut = requireNotNull(PlainText.oneLine(emoji.repeat(10), 4))
        assertEquals(emoji.repeat(3) + "…", cut)
        val family = "👩‍👧"
        assertEquals("joiners inside emoji stay", family, PlainText.oneLine(family, 10))
    }

    @Test
    fun theBridgesHiddenCharactersGoAndItsJoinersStay() {
        // Hangul and Khmer fillers, a Mongolian selector, a tag: as the bridge.
        assertEquals("ab", PlainText.oneLine("aㅤᅟ឴᠋󠁁b", 10))
        assertNull("fillers alone are blank", PlainText.oneLine("ㅤ ﾠ", 10))
        assertNull("so are joiners and selectors", PlainText.oneLine("‍️", 10))
        val persian = "می‌خواهم"
        assertEquals("the non-joiner shapes Persian", persian, PlainText.oneLine(persian, 20))
    }
}
