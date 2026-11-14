package dev.passporter.push

import java.util.Base64
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/** The bridge's own envelope (`crates/bridge/src/push/fixtures/native-envelope-vector.json`). */
internal object EnvelopeVector {
    private val json: JSONObject by lazy {
        val url = checkNotNull(javaClass.classLoader?.getResource("native-envelope-vector.json")) {
            "the vector is in crates/bridge/src/push/fixtures"
        }
        JSONObject(url.readText())
    }

    /** The key that signed it. */
    val signer: String get() = json.getString("signer")

    /** The envelope, as the companion carries it. */
    val envelope: String get() = json.getString("envelope")

    /** Its registration. */
    val registration: String get() = JSONObject(envelope).getString("registrationId")

    /** Its expiry, Unix seconds. */
    val expiresAt: Long get() = JSONObject(envelope).getLong("expiresAt")

    /** A time at which it is valid. */
    val validAt: Long get() = expiresAt - 60

    /** [envelope] with [field] set to [value]. */
    fun with(field: String, value: Any): String = JSONObject(envelope).put(field, value).toString()
}

/** Robolectric for the platform's `org.json`. */
@RunWith(RobolectricTestRunner::class)
class NativeEnvelopeTest {
    private fun open(text: String = EnvelopeVector.envelope, registration: String = EnvelopeVector.registration, at: Long = EnvelopeVector.validAt) =
        NativeEnvelope.open(text, registration, EnvelopeVector.signer, at)

    @Test
    fun opensWhatTheBridgeSignsToTheNoticeCiphertext() {
        val opened = open()
        check(opened is Opened.Ciphertext) { "refused: $opened" }
        // The notice vector's ciphertext, which WebPushVectorTest decrypts.
        assertArrayEquals(Base64.getUrlDecoder().decode(TestVector.body), opened.body)
    }

    @Test
    fun refusesAnEnvelopeForAnotherRegistration() {
        assertEquals(Opened.Refused(EnvelopeRefusal.OTHER_REGISTRATION), open(registration = "00000000-0000-4000-8000-000000000001"))
    }

    @Test
    fun refusesAnythingTheBridgeDidNotSign() {
        val body = JSONObject(EnvelopeVector.envelope).getString("body")
        val tampered = body.substring(0, body.length - 4) + (if (body.endsWith("AAAA")) "BBBB" else "AAAA")
        assertEquals(Opened.Refused(EnvelopeRefusal.FORGED), open(EnvelopeVector.with("body", tampered)))
        // A later expiry the signature does not cover.
        assertEquals(Opened.Refused(EnvelopeRefusal.FORGED), open(EnvelopeVector.with("expiresAt", EnvelopeVector.expiresAt + 1)))
        // Signed by another key: the uncompressed generator point of P-256.
        val other = "BGsX0fLhLEJH-Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU"
        assertEquals(
            Opened.Refused(EnvelopeRefusal.FORGED),
            NativeEnvelope.open(EnvelopeVector.envelope, EnvelopeVector.registration, other, EnvelopeVector.validAt),
        )
    }

    @Test
    fun refusesAnExpiredEnvelopeAndOneThatClaimsTooLongALife() {
        assertEquals(Opened.Refused(EnvelopeRefusal.EXPIRED), open(at = EnvelopeVector.expiresAt))
        // More than an hour (and ten minutes of clock skew) ahead: never one the bridge made now.
        assertEquals(Opened.Refused(EnvelopeRefusal.EXPIRED), open(at = EnvelopeVector.expiresAt - 4201))
        check(open(at = EnvelopeVector.expiresAt - 4200) is Opened.Ciphertext)
    }

    @Test
    fun refusesWhatIsNotAnEnvelope() {
        for (text in listOf("", "not json", "[]", EnvelopeVector.with("version", 2), EnvelopeVector.with("signature", "short"), "{" + " ".repeat(4096) + "}")) {
            assertEquals(text.take(40), Opened.Refused(EnvelopeRefusal.MALFORMED), open(text))
        }
    }

    @Test
    fun aSigningKeyIsAnUncompressedP256Point() {
        assertTrue(NativeEnvelope.isSigningKey(EnvelopeVector.signer))
        assertFalse(NativeEnvelope.isSigningKey("AAAA"))
        assertFalse(NativeEnvelope.isSigningKey("not base64!"))
    }
}
