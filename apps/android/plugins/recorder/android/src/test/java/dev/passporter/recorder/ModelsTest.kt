package dev.passporter.recorder

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class ModelsTest {
    @Test
    fun ids_are_lowercase_uuids() {
        assertNotNull(RecordingId.parse("9ca09978-ef89-46fd-9a85-118b84bee7b4"))
        for (bad in listOf("9CA09978-EF89-46FD-9A85-118B84BEE7B4", "../../etc/passwd", "", "9ca09978ef8946fd9a85118b84bee7b4")) {
            assertNull(bad, RecordingId.parse(bad))
        }
        assertNotNull(RecordingId.parse(RecordingId.random().value))
    }

    @Test
    fun tags_are_bounded_and_printable() {
        assertNotNull(Tag.parse("w1:p1"))
        assertNotNull(Tag.parse("x".repeat(Tag.MAX_LENGTH)))
        for (bad in listOf("", "x".repeat(Tag.MAX_LENGTH + 1), "a\nb", "a\u0000")) assertNull(Tag.parse(bad))
    }

    /** The status the PWA decodes (`crates/web/src/voice/native.rs` reads the same file). */
    @Test
    fun status_matches_the_golden_file() {
        val golden = checkNotNull(javaClass.getResource("/recorder-status-golden.json")).readText()
        val id = checkNotNull(RecordingId.parse("9ca09978-ef89-46fd-9a85-118b84bee7b4"))
        val other = checkNotNull(RecordingId.parse("0f6b1a52-3c1e-4d7a-8f2b-5e9c7d4a1b30"))
        val tag = checkNotNull(Tag.parse("w1:p1"))
        val snapshot = Snapshot(
            Active(other, tag, 4_200, 0.5f),
            listOf(Take(id, tag, 61_440, 122_880, EndReason.INTERRUPTED, 1_767_225_600_000)),
        )
        assertEquals(canonical(JSONObject(golden)), canonical(snapshot.toJson()))
        assertEquals("""{"takes":[]}""", Snapshot(null, emptyList()).toJson().toString())
    }

    /** [value] as plain Kotlin values, keys sorted and numbers as doubles, to compare JSON by content. */
    private fun canonical(value: Any?): Any? = when (value) {
        is JSONObject -> value.keys().asSequence().sorted().associateWith { canonical(value.get(it)) }
        is JSONArray -> (0 until value.length()).map { canonical(value.get(it)) }
        is Number -> value.toDouble()
        else -> value
    }

    @Test
    fun every_end_reason_round_trips() {
        for (reason in EndReason.entries) assertEquals(reason, EndReason.of(reason.wire))
        assertNull(EndReason.of("cosmic_rays"))
    }
}
