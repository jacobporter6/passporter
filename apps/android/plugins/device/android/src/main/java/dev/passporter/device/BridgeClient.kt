package dev.passporter.device

import java.io.IOException

/** The unauthenticated `/v1/auth/` calls, on the bridge at [origin]. */
internal class BridgeClient(private val origin: BridgeOrigin, private val transport: HttpTransport) {
    fun pair(request: PairRequest): PairResponse = PairResponse.parse(post("/v1/auth/pair", request.toJson()))

    fun challenge(request: ChallengeRequest): ChallengeResponse =
        ChallengeResponse.parse(post("/v1/auth/challenge", request.toJson()))

    fun session(request: SessionRequest): SessionGrant =
        SessionGrant.parse(post("/v1/auth/session", request.toJson()))

    /** One JSON `POST`; the body of a 2xx answer, or the refusal as a [DeviceAuthException]. */
    private fun post(path: String, body: String): String {
        val response = try {
            transport.send(
                HttpRequest("POST", origin.resolve(path), mapOf("Content-Type" to JSON, "Accept" to JSON), body),
            )
        } catch (e: IOException) {
            throw DeviceAuthException.Unreachable(e)
        }
        return when (response.status) {
            in 200..299 -> response.body
            401 -> throw DeviceAuthException.Refused()
            409 -> throw DeviceAuthException.TooManyDevices()
            DEVICE_UNKNOWN -> throw DeviceAuthException.Forgotten()
            429 -> throw DeviceAuthException.RateLimited(response.retryAfterSecs)
            in 500..599 -> throw DeviceAuthException.ServerError(response.status)
            else -> throw DeviceAuthException.BadResponse("status ${response.status}")
        }
    }

    companion object {
        /** `protocol::DEVICE_UNKNOWN_STATUS`: the bridge does not know the device. */
        const val DEVICE_UNKNOWN = 410
        private const val JSON = "application/json"
    }
}
