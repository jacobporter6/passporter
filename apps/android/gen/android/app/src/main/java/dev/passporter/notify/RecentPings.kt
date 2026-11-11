package dev.passporter.notify

import android.content.Context

/**
 * The pings of the last [KEEP] messages shown, so a message delivered
 * twice (a push service retry) is shown once. A ping is 128 random bits,
 * meaningless on its own; the file stays on the phone (backups exclude
 * the app's data).
 */
internal class RecentPings(context: Context) {
    private val preferences = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    /** The remembered pings, oldest first. */
    fun all(): List<String> =
        preferences.getString(KEY, null)?.split(SEPARATOR)?.filter { it.isNotEmpty() }.orEmpty()

    /** Remembers [ping], forgetting the oldest beyond [KEEP]. */
    fun add(ping: String) {
        val kept = (all() - ping + ping).takeLast(KEEP)
        preferences.edit().putString(KEY, kept.joinToString(SEPARATOR)).apply()
    }

    private companion object {
        const val FILE = "dev.passporter.notify"
        const val KEY = "recentPings"
        const val SEPARATOR = "\n"

        /** Retries come within minutes; a few dozen messages is plenty. */
        const val KEEP = 32
    }
}
