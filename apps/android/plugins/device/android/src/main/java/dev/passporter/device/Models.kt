package dev.passporter.device

import org.json.JSONException
import org.json.JSONObject

// Typed mirrors of the device-auth types in `crates/protocol/src/auth.rs`:
// the same field names (camelCase on the wire), and validation that keeps
// anything malformed from being sent or acted on. Display-only values (the
// device name the bridge reports) are dropped rather than refused when they
// do not pass this side's stricter check.

private val HEX = Regex("[0-9a-f]+")

private fun isHex(text: String, bytes: Int) = text.length == bytes * 2 && HEX.matches(text)

/** A paired device's id: `dev-` and 16 lowercase hex digits. Never a credential. */
class DeviceId private constructor(val value: String) {
    override fun equals(other: Any?) = other is DeviceId && other.value == value
    override fun hashCode() = value.hashCode()
    override fun toString() = value

    companion object {
        /** [text] as a device id, or null if it is not one. */
        fun parse(text: String): DeviceId? =
            text.removePrefix("dev-").takeIf { text.startsWith("dev-") && isHex(it, 8) }?.let { DeviceId(text) }
    }
}

/** A one-time pairing code: 8 symbols of Crockford base32, upper case, no separator. */
class PairingCode private constructor(val value: String) {
    /** Redacted: the code is a credential while it lives. */
    override fun toString() = "PairingCode(<redacted>)"

    companion object {
        private const val ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

        /** [text] (already normalized by the caller) as a code, or null. */
        fun parse(text: String): PairingCode? =
            text.takeIf { it.length == 8 && it.all(ALPHABET::contains) }?.let(::PairingCode)
    }
}

/**
 * A label for `passporter devices list`: 1 to 48 bytes, not blank, without
 * control or invisible characters. The bridge checks it too, with the
 * protocol's exact list.
 */
class DeviceName private constructor(val value: String) {
    override fun toString() = value

    companion object {
        private const val MAX_BYTES = 48

        private fun hidden(codePoint: Int): Boolean = when (Character.getType(codePoint).toByte()) {
            Character.CONTROL, Character.FORMAT, Character.UNASSIGNED, Character.PRIVATE_USE,
            Character.SURROGATE, Character.LINE_SEPARATOR, Character.PARAGRAPH_SEPARATOR -> true
            else -> false
        }

        /** [text] as a name, or null. */
        fun parse(text: String): DeviceName? = text.takeIf {
            it.isNotBlank() && it.toByteArray(Charsets.UTF_8).size <= MAX_BYTES &&
                it.codePoints().noneMatch(::hidden)
        }?.let(::DeviceName)
    }
}

/** A server challenge: 32 random bytes in lowercase hex. */
class Challenge private constructor(val value: String) {
    override fun toString() = value

    companion object {
        /** [text] as a challenge, or null. */
        fun parse(text: String): Challenge? = text.takeIf { isHex(it, 32) }?.let(::Challenge)
    }
}

/** What a session may do (`protocol::Scope`). */
enum class Scope(val wire: String) {
    /** The app and its notifications: the whole API. */
    APP("app"),

    /** Android Auto: the car-safe endpoints only, every action judged as `car`. */
    CAR("car"),
    ;

    companion object {
        /** The scope named [wire], or null. */
        fun parse(wire: String): Scope? = entries.firstOrNull { it.wire == wire }
    }
}

/** A bearer token as the bridge issued it: the RFC 6750 `b64token` charset, at most 4096 bytes. */
internal fun isBearerToken(token: String): Boolean {
    if (token.isEmpty() || token.length > 4096) return false
    val body = token.trimEnd('=')
    return body.isNotEmpty() && body.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' || it in "-._~+/" }
}

/** `POST /v1/auth/pair`. */
internal class PairRequest(val code: PairingCode, val publicKey: ByteArray, val name: DeviceName?) {
    fun toJson(): String = JSONObject().apply {
        put("code", code.value)
        put("publicKey", Hex.encode(publicKey))
        name?.let { put("name", it.value) }
    }.toString()
}

/** `POST /v1/auth/challenge`. */
internal class ChallengeRequest(val deviceId: DeviceId) {
    fun toJson(): String = JSONObject().put("deviceId", deviceId.value).toString()
}

/** `POST /v1/auth/session`. `scope` is left out for [Scope.APP], as the protocol serializes it. */
internal class SessionRequest(
    val deviceId: DeviceId,
    val challenge: Challenge,
    val signature: ByteArray,
    val scope: Scope,
) {
    fun toJson(): String = JSONObject().apply {
        put("deviceId", deviceId.value)
        put("challenge", challenge.value)
        put("signature", Hex.encode(signature))
        if (scope != Scope.APP) put("scope", scope.wire)
    }.toString()
}

/** The answer to a [PairRequest]. */
internal class PairResponse(val deviceId: DeviceId) {
    companion object {
        fun parse(body: String): PairResponse = Json(body).run {
            PairResponse(DeviceId.parse(string("deviceId")) ?: invalid("deviceId"))
        }
    }
}

/** The answer to a [ChallengeRequest]. */
internal class ChallengeResponse(val challenge: Challenge, val expiresInSecs: Long) {
    companion object {
        fun parse(body: String): ChallengeResponse = Json(body).run {
            ChallengeResponse(
                Challenge.parse(string("challenge")) ?: invalid("challenge"),
                seconds("expiresInSecs"),
            )
        }
    }
}

/** The answer to a [SessionRequest] (`protocol::SessionGrant`). */
internal class SessionGrant(
    val token: String,
    val expiresInSecs: Long,
    val deviceName: String?,
    val scope: Scope,
) {
    override fun toString() = "SessionGrant(<redacted>, $expiresInSecs s, $scope)"

    companion object {
        fun parse(body: String): SessionGrant = Json(body).run {
            SessionGrant(
                string("token").takeIf(::isBearerToken) ?: invalid("token"),
                seconds("expiresInSecs"),
                // Display only: a name this side would not accept (the
                // protocol allows joiners, say) is dropped, never fatal.
                optionalString("deviceName")?.let { DeviceName.parse(it)?.value },
                optionalString("scope")?.let { Scope.parse(it) ?: invalid("scope") } ?: Scope.APP,
            )
        }
    }
}

/**
 * Strict reads of one JSON object: a field must have the expected JSON type
 * (no number-to-string coercion). Unknown fields are ignored, as the Rust
 * clients' serde does for these answers.
 */
private class Json(body: String) {
    private val obj: JSONObject = try {
        JSONObject(body)
    } catch (e: JSONException) {
        throw DeviceAuthException.BadResponse("not a JSON object")
    }

    fun invalid(field: String): Nothing = throw DeviceAuthException.BadResponse("invalid $field")

    fun string(field: String): String = obj.opt(field) as? String ?: invalid(field)

    fun optionalString(field: String): String? = when (val value = obj.opt(field)) {
        null, JSONObject.NULL -> null
        is String -> value
        else -> invalid(field)
    }

    fun seconds(field: String): Long = when (val value = obj.opt(field)) {
        is Int -> value.toLong()
        is Long -> value
        else -> null
    }?.takeIf { it >= 0 } ?: invalid(field)
}
