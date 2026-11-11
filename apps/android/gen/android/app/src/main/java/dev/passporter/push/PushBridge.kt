package dev.passporter.push

import dev.passporter.device.DeviceAuth
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.device.Scope
import org.json.JSONException
import org.json.JSONObject

/** The bridge's VAPID public key (`GET /v1/push/key`), as far as it could be learnt. */
sealed interface VapidKey {
    /** The key: an uncompressed P-256 point, base64url without padding (87 characters). */
    data class Known(
        /** The key, as the distributor wants it. */
        val value: String,
    ) : VapidKey

    /** The bridge has push turned off (404). */
    data object PushOff : VapidKey

    /** No usable answer now (offline, a bridge error): try again later. */
    data object Unknown : VapidKey

    companion object {
        /** The key in a `GET /v1/push/key` answer of [status] with [body]. */
        fun from(status: Int, body: String): VapidKey = when (status) {
            200 -> try {
                JSONObject(body).optString("vapidPublicKey").trimEnd('=')
                    .takeIf { KEY.matches(it) }?.let(::Known) ?: Unknown
            } catch (e: JSONException) {
                Unknown
            }
            404 -> PushOff
            else -> Unknown
        }

        /** 65 bytes in base64url, starting with 0x04 (so with `B`). */
        private val KEY = Regex("^B[A-Za-z0-9_-]{86}$")
    }
}

/**
 * What push needs from the bridge, all as this paired device. Blocking:
 * call from a worker thread.
 */
interface PushBridge {
    /**
     * This app's device id (`DeviceId.value`), or null when it is not paired.
     *
     * @throws dev.passporter.device.DeviceAuthException when the Keystore fails.
     */
    fun deviceId(): String?

    /** The bridge's VAPID key, for the distributor. Never throws. */
    fun vapidKey(): VapidKey

    /**
     * `POST /v1/push/subscriptions` with [json].
     *
     * @throws dev.passporter.device.DeviceAuthException when there is no session.
     */
    fun subscribe(json: String): HttpResponse

    /**
     * `DELETE /v1/push/subscriptions` with [json].
     *
     * @throws dev.passporter.device.DeviceAuthException when there is no session.
     */
    fun unsubscribe(json: String): HttpResponse
}

/** [PushBridge] through the device plugin, with app-scope sessions. */
class DeviceAuthBridge(private val auth: DeviceAuth) : PushBridge {
    override fun deviceId(): String? = auth.status().deviceId?.value

    override fun vapidKey(): VapidKey = try {
        val response = auth.send(Scope.APP, Method.GET, KEY_PATH)
        VapidKey.from(response.status, response.body)
    } catch (e: DeviceAuthException) {
        VapidKey.Unknown
    }

    override fun subscribe(json: String): HttpResponse = auth.send(Scope.APP, Method.POST, SUBSCRIPTIONS_PATH, json)

    override fun unsubscribe(json: String): HttpResponse =
        auth.send(Scope.APP, Method.DELETE, SUBSCRIPTIONS_PATH, json)

    private companion object {
        const val KEY_PATH = "/v1/push/key"
        const val SUBSCRIPTIONS_PATH = "/v1/push/subscriptions"
    }
}
