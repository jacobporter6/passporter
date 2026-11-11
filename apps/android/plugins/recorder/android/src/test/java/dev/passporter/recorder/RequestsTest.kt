package dev.passporter.recorder

import app.tauri.PermissionState
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class RequestsTest {
    private val id = "9ca09978-ef89-46fd-9a85-118b84bee7b4"

    @Test
    fun start_takes_a_tag_and_a_cap() {
        val start = Requests.start(JSONObject("""{"tag":"w1:p1","maxMs":295000}"""))
        assertEquals("w1:p1", start.tag.value)
        assertEquals(295_000L, start.maxMs)
        assertEquals(295_000L, Requests.start(JSONObject("""{"tag":"w1:p1","maxMs":295000.0}""")).maxMs)
        for (bad in listOf(
            """{"tag":"w1:p1"}""",
            """{"maxMs":295000}""",
            """{"tag":"","maxMs":295000}""",
            """{"tag":"w1:p1","maxMs":"295000"}""",
            """{"tag":"w1:p1","maxMs":295000.5}""",
            """{"tag":"w1:p1","maxMs":999}""",
            """{"tag":"w1:p1","maxMs":${Requests.MAX_TAKE_MS + 1}}""",
            """{"tag":7,"maxMs":295000}""",
            """{"tag":"w1:p1","maxMs":295000,"path":"/sdcard"}""",
        )) {
            assertThrows(bad, RecorderException.InvalidInput::class.java) { Requests.start(JSONObject(bad)) }
        }
    }

    @Test
    fun ids_must_be_recording_ids() {
        assertEquals(id, Requests.id(JSONObject("""{"id":"$id"}""")).value)
        for (bad in listOf("""{}""", """{"id":"../x"}""", """{"id":"$id","extra":1}""", """{"id":1}""")) {
            assertThrows(bad, RecorderException.InvalidInput::class.java) { Requests.id(JSONObject(bad)) }
        }
    }

    @Test
    fun reads_are_bounded() {
        val read = Requests.read(JSONObject("""{"id":"$id","offset":524288,"length":524288}"""))
        assertEquals(524_288L, read.offset)
        assertEquals(Requests.MAX_READ, read.length)
        for (bad in listOf(
            """{"id":"$id","offset":-1,"length":10}""",
            """{"id":"$id","offset":0,"length":0}""",
            """{"id":"$id","offset":0,"length":${Requests.MAX_READ + 1}}""",
            """{"id":"$id","offset":0}""",
        )) {
            assertThrows(bad, RecorderException.InvalidInput::class.java) { Requests.read(JSONObject(bad)) }
        }
    }

    @Test
    fun status_takes_no_arguments() {
        Requests.only(JSONObject("{}"), emptySet())
        assertThrows(RecorderException.InvalidInput::class.java) { Requests.only(JSONObject("""{"all":true}"""), emptySet()) }
    }

    @Test
    fun the_microphone_is_asked_for_once_per_start() {
        assertEquals(MicAccess.RECORD, MicAccess.next(PermissionState.GRANTED, asked = false))
        assertEquals(MicAccess.RECORD, MicAccess.next(PermissionState.GRANTED, asked = true))
        for (state in listOf(PermissionState.PROMPT, PermissionState.PROMPT_WITH_RATIONALE, PermissionState.DENIED, null)) {
            assertEquals("$state", MicAccess.ASK, MicAccess.next(state, asked = false))
            assertEquals("$state, after the prompt", MicAccess.REFUSE, MicAccess.next(state, asked = true))
        }
    }

    @Test
    fun only_a_real_loss_of_focus_ends_a_take() {
        assertEquals(true, SystemAudioFocus.endsTake(android.media.AudioManager.AUDIOFOCUS_LOSS))
        assertEquals(true, SystemAudioFocus.endsTake(android.media.AudioManager.AUDIOFOCUS_LOSS_TRANSIENT))
        assertEquals(false, SystemAudioFocus.endsTake(android.media.AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK))
        assertEquals(false, SystemAudioFocus.endsTake(android.media.AudioManager.AUDIOFOCUS_GAIN))
    }
}
