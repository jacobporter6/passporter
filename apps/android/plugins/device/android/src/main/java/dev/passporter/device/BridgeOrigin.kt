package dev.passporter.device

import java.io.File
import java.io.IOException
import java.net.URI
import java.net.URISyntaxException

/**
 * The paired bridge's HTTPS origin, `https://host[:port]`: where every
 * device-auth request goes.
 *
 * Native code and the webview must agree on it, so it is not configured
 * here: the shell saves it when you connect (`apps/android/src/store.rs`,
 * one line in the file [FILE_NAME] in the app data directory, which Tauri
 * resolves to `Context.getDataDir()`), and [saved] reads that same file.
 */
class BridgeOrigin private constructor(private val origin: String) {
    /** The URL of [path] (absolute, starting with `/`) on this origin. */
    fun resolve(path: String): String {
        require(path.startsWith("/") && !path.startsWith("//")) { "not an absolute path: $path" }
        return origin + path
    }

    override fun equals(other: Any?) = other is BridgeOrigin && other.origin == origin
    override fun hashCode() = origin.hashCode()
    override fun toString() = origin

    companion object {
        /** The shell's file (`store::FILE_NAME`). */
        const val FILE_NAME = "bridge-origin"

        /**
         * [text] as an origin, or null. Only what the shell writes is
         * accepted: `https`, a host the shell accepts (`isBridgeHost`), an
         * optional port, and nothing else. `bridge-origins.json` holds the
         * cases both sides check.
         */
        fun parse(text: String): BridgeOrigin? {
            val uri = try {
                URI(text.trim())
            } catch (e: URISyntaxException) {
                return null
            }
            val host = uri.host?.lowercase() ?: return null
            val plain = uri.scheme == "https" && uri.rawUserInfo == null && isBridgeHost(host) &&
                uri.rawPath.isNullOrEmpty() && uri.rawQuery == null && uri.rawFragment == null
            if (!plain) return null
            val port = if (uri.port == -1) "" else ":${uri.port}"
            return BridgeOrigin("https://$host$port")
        }

        /** An RFC 1123 label: letters, digits and inner `-`. */
        private const val LABEL = "[a-z0-9](?:[a-z0-9-]*[a-z0-9])?"

        /** The last label also starts with a letter (an all-digit one would be an address). */
        private const val TOP = "[a-z](?:[a-z0-9-]*[a-z0-9])?"

        /** An IPv4 octet as the shell writes it: 0 to 255, no leading zero. */
        private const val OCTET = "(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])"

        private val HOST_NAME = Regex("(?:$LABEL\\.)*$TOP")
        private val IPV4 = Regex("$OCTET(?:\\.$OCTET){3}")

        /**
         * Whether [host] (lowercase) is one the shell saves
         * (`apps/android/src/origin.rs`, `is_dns_name`): an RFC 1123 host
         * name that is not `localhost` or under it, or an IPv4 address in
         * its plain dotted form.
         */
        private fun isBridgeHost(host: String): Boolean =
            IPV4.matches(host) ||
                (HOST_NAME.matches(host) && host != "localhost" && !host.endsWith(".localhost"))

        /** The origin the shell saved in [dataDir], or null before the app connects. */
        fun saved(dataDir: File): BridgeOrigin? = try {
            parse(File(dataDir, FILE_NAME).readText())
        } catch (e: IOException) {
            null
        }
    }
}
