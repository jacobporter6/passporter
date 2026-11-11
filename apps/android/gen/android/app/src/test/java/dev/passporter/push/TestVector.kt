package dev.passporter.push

import org.json.JSONObject

/**
 * The bridge's Web Push vector (`crates/bridge/src/push/fixtures/`, a test
 * resource): RFC 8291's example receiver keys, public test values, and a
 * notice message the bridge sealed to them. Needs Robolectric (`org.json`).
 */
internal object TestVector {
    private val json: JSONObject by lazy {
        val url = checkNotNull(javaClass.classLoader?.getResource("webpush-notice-vector.json")) {
            "the vector is in crates/bridge/src/push/fixtures"
        }
        JSONObject(url.readText())
    }

    /** The receiver's P-256 private key (32 bytes, base64url). */
    val uaPrivate: String get() = json.getString("uaPrivate")

    /** The receiver's public key, uncompressed (base64url). */
    val uaPublic: String get() = json.getString("uaPublic")

    /** The receiver's auth secret (base64url). */
    val auth: String get() = json.getString("auth")

    /** The message, as the bridge serialized it. */
    val plaintext: String get() = json.getString("plaintext")

    /** The `aes128gcm` body the bridge produced (base64url). */
    val body: String get() = json.getString("body")
}
