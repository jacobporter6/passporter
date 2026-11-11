package dev.passporter.push

import java.net.URI
import java.net.URISyntaxException
import java.util.Base64
import org.json.JSONObject

/**
 * What the UnifiedPush distributor gave this app: the endpoint the bridge
 * posts to, and the Web Push public keys (RFC 8291) the bridge encrypts
 * to. The private key stays with the connector (`DefaultKeyManager`:
 * sealed with an Android Keystore key, in app-private storage).
 *
 * The endpoint is a capability (anyone holding it can push to the phone),
 * so `toString` shows its host only.
 */
class Registration private constructor(
    /** The push endpoint URL. */
    val endpoint: String,
    /** The P-256 public key, uncompressed, base64url without padding. */
    val p256dh: String,
    /** The 16-byte auth secret, base64url without padding. */
    val auth: String,
) {
    /**
     * `POST /v1/push/subscriptions`: the browser's `PushSubscription` shape
     * plus `"payload": "notice"`, so the bridge sends notices, not the bare
     * ping (`crates/bridge/src/push/subscription.rs`).
     */
    fun subscribeJson(): String = JSONObject()
        .put("endpoint", endpoint)
        .put("keys", JSONObject().put("p256dh", p256dh).put("auth", auth))
        .put("payload", "notice")
        .toString()

    override fun equals(other: Any?) =
        other is Registration && other.endpoint == endpoint && other.p256dh == p256dh && other.auth == auth

    override fun hashCode() = endpoint.hashCode()

    override fun toString() = "Registration(${host(endpoint)}/…)"

    companion object {
        /**
         * A registration, or null when the distributor's answer is not one
         * the bridge could use: an endpoint that is not `https`, or keys of
         * the wrong shape. (The bridge checks the rest: the host must be a
         * known push service or `[push] ntfy_host`, at port 443.)
         */
        fun of(endpoint: String, p256dh: String, auth: String): Registration? {
            val https = try {
                URI(endpoint).let { it.scheme == "https" && !it.host.isNullOrEmpty() }
            } catch (e: URISyntaxException) {
                false
            }
            val point = decode(p256dh)
            val pointOk = point != null && point.size == POINT_BYTES && point[0] == UNCOMPRESSED
            val authOk = decode(auth)?.size == AUTH_BYTES
            if (!https || !pointOk || !authOk) return null
            return Registration(endpoint, p256dh.trimEnd('='), auth.trimEnd('='))
        }

        /** `DELETE /v1/push/subscriptions`: which one to drop. */
        fun unsubscribeJson(endpoint: String): String = JSONObject().put("endpoint", endpoint).toString()

        /** The host of [endpoint], for logs. */
        fun host(endpoint: String): String = try {
            URI(endpoint).host ?: "?"
        } catch (e: URISyntaxException) {
            "?"
        }

        private fun decode(text: String): ByteArray? = try {
            Base64.getUrlDecoder().decode(text.trimEnd('='))
        } catch (e: IllegalArgumentException) {
            null
        }

        private const val POINT_BYTES = 65
        private const val UNCOMPRESSED: Byte = 0x04
        private const val AUTH_BYTES = 16
    }
}
