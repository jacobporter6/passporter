package dev.passporter.device

import java.io.IOException

/**
 * Why pairing or getting a session failed. [code] is stable: the PWA maps it
 * to its own errors (`crates/web/src/pair/native.rs`).
 */
sealed class DeviceAuthException(
    /** A stable, camelCase name for the failure. */
    val code: String,
    message: String,
    cause: Throwable? = null,
) : Exception(message, cause) {
    /** The app has no bridge address yet (the Connect page has not run). */
    class NoBridge : DeviceAuthException("noBridge", "the app has no bridge yet")

    /** This app is not paired (or was paired with another bridge). */
    class NotPaired : DeviceAuthException("notPaired", "this app is not paired")

    /**
     * A request meant to go as one device ([DeviceAuth.send]'s `device`)
     * while the app is paired as another (or not at all): nothing was sent.
     */
    class OtherDevice : DeviceAuthException("otherDevice", "the app is no longer paired as that device")

    /**
     * The bridge does not know this device (`410 Gone`): revoked, or never
     * paired. The pairing has been forgotten here too.
     */
    class Forgotten : DeviceAuthException("forgotten", "the bridge no longer knows this device; pair it again")

    /**
     * `401`: a wrong, expired or used code, or a challenge or signature the
     * bridge refused even on a second try. Says nothing about the device.
     */
    class Refused : DeviceAuthException("refused", "the bridge refused the request")

    /** `429`: too many attempts. [retryAfterSecs] is the bridge's hint, if any. */
    class RateLimited(val retryAfterSecs: Long?) :
        DeviceAuthException("rateLimited", "too many attempts; wait a minute and try again")

    /** `409`: the bridge has as many devices as it allows. */
    class TooManyDevices : DeviceAuthException(
        "tooManyDevices",
        "the bridge has too many paired devices; revoke one with `passporter devices revoke`",
    )

    /** No answer: offline, DNS, TLS or a timeout. Worth retrying later. */
    class Unreachable(cause: IOException) :
        DeviceAuthException("unreachable", "the bridge is unreachable: ${cause.message ?: cause.javaClass.simpleName}", cause)

    /** `5xx`: the bridge (or its proxy) failed. Worth retrying later. */
    class ServerError(val status: Int) : DeviceAuthException("serverError", "the bridge failed (status $status)")

    /** An answer the client does not understand: another status, or a malformed body. */
    class BadResponse(detail: String) : DeviceAuthException("badResponse", "unexpected answer from the bridge: $detail")

    /** The Keystore could not make, find or use the device key. */
    class Keystore(cause: Throwable) :
        DeviceAuthException("keystore", "the device key failed: ${cause.message ?: cause.javaClass.simpleName}", cause)

    /** App-private storage could not be written (the pairing record). */
    class Storage(cause: Throwable) :
        DeviceAuthException("storage", "could not save the pairing: ${cause.message ?: cause.javaClass.simpleName}", cause)

    /** The caller passed something that is not valid (a malformed code or name). */
    class InvalidInput(field: String) : DeviceAuthException("invalidInput", "invalid $field")
}
