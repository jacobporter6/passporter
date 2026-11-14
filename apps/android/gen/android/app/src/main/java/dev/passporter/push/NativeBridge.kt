package dev.passporter.push

import android.content.Context
import dev.passporter.device.BridgeOrigin
import dev.passporter.device.DeviceAuth
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.device.Scope
import dev.passporter.device.SessionToken
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.io.IOException
import java.io.InputStream
import java.net.URL
import javax.net.ssl.HttpsURLConnection

/**
 * One open `GET /v1/push/native/<id>/stream`: its [status], and while it
 * is 200, its lines. Closing it ends the connection (also from another
 * thread, to wake a blocked read).
 */
class NativeStream(
    /** The HTTP status. */
    val status: Int,
    private val input: InputStream?,
    private val onClose: () -> Unit,
) : Closeable {
    /**
     * The next line, without its newline, or null at the end. A line longer
     * than [MAX_LINE] bytes ends the stream as an [IOException]: the bridge
     * never sends one.
     */
    fun nextLine(): String? {
        val source = input ?: return null
        val line = ByteArrayOutputStream()
        while (true) {
            val byte = source.read()
            if (byte < 0) return if (line.size() == 0) null else line.toString(Charsets.UTF_8.name())
            if (byte == '\n'.code) return line.toString(Charsets.UTF_8.name())
            if (line.size() >= MAX_LINE) throw IOException("stream line too long")
            line.write(byte)
        }
    }

    override fun close() = onClose()

    companion object {
        /** Longest event line: a full envelope, JSON-escaped, plus ntfy's fields. */
        const val MAX_LINE = 16 * 1024
    }
}

/** What the built-in receiver needs from the bridge, as this paired device. Blocking. */
interface NativeBridge {
    /**
     * This app's device id, or null when it is not paired.
     *
     * @throws DeviceAuthException when the Keystore fails.
     */
    fun deviceId(): DeviceId?

    /**
     * `POST /v1/push/native` with [json].
     *
     * @throws DeviceAuthException when there is no session.
     */
    fun register(json: String): HttpResponse

    /**
     * `DELETE /v1/push/subscriptions` with [json], as [device] only.
     *
     * @throws DeviceAuthException when there is no session for [device].
     */
    fun unsubscribe(json: String, device: DeviceId): HttpResponse

    /**
     * Opens [path] as a stream, as [device] only (a 401 is retried once with
     * a fresh token).
     *
     * @throws DeviceAuthException when there is no session for [device].
     * @throws IOException when the bridge cannot be reached.
     */
    fun openStream(path: String, device: DeviceId): NativeStream
}

/** [NativeBridge] through the device plugin, with app-scope sessions. */
class DeviceAuthNativeBridge(private val context: Context, private val auth: DeviceAuth) : NativeBridge {
    override fun deviceId(): DeviceId? = auth.status().deviceId

    override fun register(json: String): HttpResponse = auth.send(Scope.APP, Method.POST, REGISTER_PATH, json)

    override fun unsubscribe(json: String, device: DeviceId): HttpResponse =
        auth.send(Scope.APP, Method.DELETE, SUBSCRIPTIONS_PATH, json, device)

    override fun openStream(path: String, device: DeviceId): NativeStream {
        val url = BridgeOrigin.saved(context.dataDir)?.resolve(path) ?: throw DeviceAuthException.NoBridge()
        val first = auth.token(Scope.APP, device)
        val stream = connect(url, first)
        if (stream.status != 401) return stream
        stream.close()
        auth.invalidate(first)
        return connect(url, auth.token(Scope.APP, device))
    }

    private fun connect(url: String, token: SessionToken): NativeStream {
        val http = URL(url).openConnection() as HttpsURLConnection
        try {
            http.instanceFollowRedirects = false
            http.useCaches = false
            http.connectTimeout = CONNECT_TIMEOUT_MS
            // The companion sends a keepalive at least every 45 s.
            http.readTimeout = READ_TIMEOUT_MS
            http.setRequestProperty("Authorization", token.authorization())
            http.setRequestProperty("Accept", "application/x-ndjson")
            val status = http.responseCode
            val input = if (status == 200) http.inputStream.buffered() else null
            return NativeStream(status, input) { http.disconnect() }
        } catch (e: IOException) {
            http.disconnect()
            throw e
        }
    }

    private companion object {
        const val REGISTER_PATH = "/v1/push/native"
        const val SUBSCRIPTIONS_PATH = "/v1/push/subscriptions"
        const val CONNECT_TIMEOUT_MS = 15_000
        const val READ_TIMEOUT_MS = 100_000
    }
}
