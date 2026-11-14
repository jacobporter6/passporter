package dev.passporter.push

import java.math.BigInteger
import java.security.AlgorithmParameters
import java.security.GeneralSecurityException
import java.security.KeyFactory
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import java.security.spec.ECParameterSpec
import java.security.spec.ECPoint
import java.security.spec.ECPublicKeySpec
import java.util.Base64
import org.json.JSONException
import org.json.JSONObject

/** Why an envelope was not opened ([NativeEnvelope.open]). */
enum class EnvelopeRefusal {
    /** Not an envelope of a version this app reads (malformed, too large, unknown version). */
    MALFORMED,

    /** For another registration than this app's. */
    OTHER_REGISTRATION,

    /** Past its expiry, or claiming one further away than the bridge ever sets. */
    EXPIRED,

    /** The signature does not verify with the key the bridge gave at registration. */
    FORGED,
}

/** What [NativeEnvelope.open] made of a message. */
sealed interface Opened {
    /** Verified: the RFC 8291 ciphertext inside, for the receiver's keys. */
    class Ciphertext(
        /** The `aes128gcm` body. */
        val body: ByteArray,
    ) : Opened

    /** Refused, unread. */
    data class Refused(
        /** Why. */
        val reason: EnvelopeRefusal,
    ) : Opened
}

/**
 * The built-in transport's envelope (`crates/bridge/src/push/native.rs`):
 * the notice ciphertext with the registration it is for and an expiry,
 * signed by the bridge (ECDSA P-256 over a domain-separated text). Checked
 * before anything is decrypted, so the companion that stores and forwards
 * it cannot make the app show a notice, even knowing the receiver's public
 * keys. Pure, for the JVM tests (`NativeEnvelopeTest` opens the bridge's
 * own vector).
 */
object NativeEnvelope {
    /** Largest envelope the bridge sends, in characters. */
    const val MAX_LENGTH = 4096

    /** How far past now the bridge sets the expiry (one hour), plus room for clock skew. */
    private const val MAX_LIFETIME_SECS = 3600L + 600L

    /** The version this app reads. */
    private const val VERSION = 1

    /** What the signature covers; the bridge's `signing_input`. */
    private const val DOMAIN = "passporter-notice-v1"

    /**
     * Opens [text] for [registration], checking it against [signingKey]
     * (the uncompressed P-256 point, base64url, from registration) at
     * [nowSecs].
     */
    fun open(text: String, registration: String, signingKey: String, nowSecs: Long): Opened {
        if (text.length > MAX_LENGTH) return refused(EnvelopeRefusal.MALFORMED)
        val envelope = try {
            JSONObject(text)
        } catch (e: JSONException) {
            return refused(EnvelopeRefusal.MALFORMED)
        }
        val version = envelope.optInt("version", -1)
        val id = envelope.optString("registrationId")
        val expires = envelope.optLong("expiresAt", -1)
        val body = envelope.optString("body")
        val signature = decode(envelope.optString("signature"))
        if (version != VERSION || expires < 0 || body.isEmpty() || signature?.size != 64) {
            return refused(EnvelopeRefusal.MALFORMED)
        }
        if (id != registration) return refused(EnvelopeRefusal.OTHER_REGISTRATION)
        if (expires <= nowSecs || expires > nowSecs + MAX_LIFETIME_SECS) return refused(EnvelopeRefusal.EXPIRED)
        val input = "$DOMAIN\n$id\n$expires\n$body".toByteArray(Charsets.UTF_8)
        if (!verifies(signingKey, input, signature)) return refused(EnvelopeRefusal.FORGED)
        return decode(body)?.let(Opened::Ciphertext) ?: refused(EnvelopeRefusal.MALFORMED)
    }

    /** Whether [key] is an uncompressed P-256 point in base64url (65 bytes, `0x04` first). */
    fun isSigningKey(key: String): Boolean = decode(key)?.let { it.size == 65 && it[0] == 4.toByte() } == true

    private fun verifies(key: String, input: ByteArray, signature: ByteArray): Boolean = try {
        val point = decode(key)?.takeIf { it.size == 65 && it[0] == 4.toByte() } ?: return false
        val curve = AlgorithmParameters.getInstance("EC")
            .apply { init(ECGenParameterSpec("secp256r1")) }
            .getParameterSpec(ECParameterSpec::class.java)
        val x = BigInteger(1, point.copyOfRange(1, 33))
        val y = BigInteger(1, point.copyOfRange(33, 65))
        // generatePublic checks the point is on the curve.
        val public = KeyFactory.getInstance("EC").generatePublic(ECPublicKeySpec(ECPoint(x, y), curve))
        Signature.getInstance("SHA256withECDSA").run {
            initVerify(public)
            update(input)
            verify(der(signature))
        }
    } catch (e: GeneralSecurityException) {
        false
    } catch (e: IllegalArgumentException) {
        false
    }

    /** The bridge's fixed-width `r || s` as the ASN.1 DER `Signature` reads. */
    private fun der(raw: ByteArray): ByteArray {
        fun integer(half: ByteArray): ByteArray {
            val value = BigInteger(1, half).toByteArray()
            return byteArrayOf(0x02, value.size.toByte()) + value
        }
        val fields = integer(raw.copyOfRange(0, 32)) + integer(raw.copyOfRange(32, 64))
        return byteArrayOf(0x30, fields.size.toByte()) + fields
    }

    private fun decode(text: String): ByteArray? = try {
        Base64.getUrlDecoder().decode(text.trimEnd('='))
    } catch (e: IllegalArgumentException) {
        null
    }

    private fun refused(reason: EnvelopeRefusal) = Opened.Refused(reason)
}
