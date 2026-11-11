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
         * accepted: `https`, a host, an optional port, and nothing else.
         */
        fun parse(text: String): BridgeOrigin? {
            val uri = try {
                URI(text.trim())
            } catch (e: URISyntaxException) {
                return null
            }
            val plain = uri.scheme == "https" && uri.rawUserInfo == null && !uri.host.isNullOrEmpty() &&
                uri.rawPath.isNullOrEmpty() && uri.rawQuery == null && uri.rawFragment == null
            if (!plain) return null
            val port = if (uri.port == -1) "" else ":${uri.port}"
            return BridgeOrigin("https://${uri.host.lowercase()}$port")
        }

        /** The origin the shell saved in [dataDir], or null before the app connects. */
        fun saved(dataDir: File): BridgeOrigin? = try {
            parse(File(dataDir, FILE_NAME).readText())
        } catch (e: IOException) {
            null
        }
    }
}
