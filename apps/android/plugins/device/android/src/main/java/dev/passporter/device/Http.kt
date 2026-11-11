package dev.passporter.device

import java.io.IOException
import java.io.InputStream
import java.net.URL
import javax.net.ssl.HttpsURLConnection

/** One HTTP request to the bridge. `toString` leaves the headers out: one may be a token. */
class HttpRequest(
    val method: String,
    val url: String,
    val headers: Map<String, String>,
    val body: String?,
) {
    override fun toString() = "$method $url"
}

/** The bridge's answer: status, body (empty when there is none) and `Retry-After` in seconds. */
class HttpResponse(val status: Int, val body: String, val retryAfterSecs: Long? = null) {
    /** Whether the status is 2xx. */
    val isSuccess: Boolean get() = status in 200..299
}

/** Sends requests. Throws [IOException] when there is no answer. */
fun interface HttpTransport {
    fun send(request: HttpRequest): HttpResponse
}

/**
 * [HttpTransport] over the platform's `HttpsURLConnection`, so no HTTP
 * library comes into the APK. The app's network security config applies:
 * HTTPS only, system CAs only. Redirects are not followed (the bridge never
 * sends one, and a token must not follow it elsewhere).
 */
internal class UrlConnectionTransport : HttpTransport {
    override fun send(request: HttpRequest): HttpResponse {
        val connection = URL(request.url).openConnection() as? HttpsURLConnection
            ?: throw IOException("not an https URL")
        try {
            connection.instanceFollowRedirects = false
            connection.connectTimeout = CONNECT_TIMEOUT_MS
            connection.readTimeout = READ_TIMEOUT_MS
            connection.requestMethod = request.method
            connection.useCaches = false
            request.headers.forEach(connection::setRequestProperty)
            request.body?.let { body ->
                val bytes = body.toByteArray(Charsets.UTF_8)
                connection.doOutput = true
                connection.setFixedLengthStreamingMode(bytes.size)
                connection.outputStream.use { it.write(bytes) }
            }
            val status = connection.responseCode
            val stream = if (status in 200..299) connection.inputStream else connection.errorStream
            val body = stream?.use(::readLimited) ?: ""
            val retryAfter = connection.getHeaderField("Retry-After")?.trim()?.toLongOrNull()
            return HttpResponse(status, body, retryAfter)
        } finally {
            connection.disconnect()
        }
    }

    private fun readLimited(stream: InputStream): String {
        val out = java.io.ByteArrayOutputStream()
        val buffer = ByteArray(8192)
        while (true) {
            val read = stream.read(buffer)
            if (read < 0) break
            out.write(buffer, 0, read)
            if (out.size() > MAX_BODY_BYTES) throw IOException("response too large")
        }
        return out.toString(Charsets.UTF_8.name())
    }

    private companion object {
        const val CONNECT_TIMEOUT_MS = 10_000
        const val READ_TIMEOUT_MS = 20_000
        const val MAX_BODY_BYTES = 1 shl 20
    }
}
