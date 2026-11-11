package dev.passporter.push

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** Robolectric for the platform's `org.json`. */
@RunWith(RobolectricTestRunner::class)
class RegistrationTest {
    @Test
    fun subscribesForNoticesInTheBridgesShape() {
        val registration = checkNotNull(Registration.of(ENDPOINT, P256DH, "$AUTH=="))
        val json = JSONObject(registration.subscribeJson())
        // Exactly what crates/bridge/src/push/subscription.rs accepts (it
        // refuses unknown fields).
        assertEquals(setOf("endpoint", "keys", "payload"), json.keys().asSequence().toSet())
        assertEquals(ENDPOINT, json.getString("endpoint"))
        assertEquals("notice", json.getString("payload"))
        val keys = json.getJSONObject("keys")
        assertEquals(setOf("p256dh", "auth"), keys.keys().asSequence().toSet())
        assertEquals(P256DH, keys.getString("p256dh"))
        assertEquals("padding dropped", AUTH, keys.getString("auth"))
        assertEquals(
            JSONObject().put("endpoint", ENDPOINT).toString(),
            Registration.unsubscribeJson(ENDPOINT),
        )
    }

    @Test
    fun refusesWhatTheBridgeCouldNotUse() {
        assertNotNull(Registration.of(ENDPOINT, P256DH, AUTH))
        assertNull(Registration.of("http://push.example.org/up/abc", P256DH, AUTH))
        assertNull(Registration.of("not a url", P256DH, AUTH))
        assertNull(Registration.of(ENDPOINT, P256DH.drop(4), AUTH))
        assertNull("compressed point", Registration.of(ENDPOINT, "A" + P256DH.drop(1), AUTH))
        assertNull(Registration.of(ENDPOINT, P256DH, AUTH.drop(4)))
        assertNull(Registration.of(ENDPOINT, P256DH, "not base64!"))
    }

    @Test
    fun neverShowsTheEndpointOrKeys() {
        val shown = checkNotNull(Registration.of(ENDPOINT, P256DH, AUTH)).toString()
        assertEquals("Registration(push.example.org/…)", shown)
        assertFalse(shown.contains("abc"))
    }

    @Test
    fun readsTheBridgesVapidKey() {
        val key = "B" + "A".repeat(86)
        assertEquals(VapidKey.Known(key), VapidKey.from(200, """{"vapidPublicKey":"$key"}"""))
        assertEquals(VapidKey.PushOff, VapidKey.from(404, ""))
        assertEquals(VapidKey.Unknown, VapidKey.from(200, """{"vapidPublicKey":"short"}"""))
        assertEquals(VapidKey.Unknown, VapidKey.from(200, "not json"))
        assertEquals(VapidKey.Unknown, VapidKey.from(502, ""))
    }

    companion object {
        const val ENDPOINT = "https://push.example.org/up/abc?up=1"

        /** RFC 8291's example receiver key: a public test value. */
        const val P256DH =
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"
        const val AUTH = "BTBZMqHH6r4Tts7J_aSIgg"
    }
}
