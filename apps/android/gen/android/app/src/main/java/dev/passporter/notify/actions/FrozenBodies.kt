package dev.passporter.notify.actions

import android.content.Context

/**
 * The frozen `POST /v1/actions` body of each job, by idempotency key. Once
 * a body has been sent, every retry sends it again unchanged: the bridge
 * replays its first result for the key (or refuses another body under it).
 */
interface FrozenBodies {
    /** The body frozen for [key], if any. */
    fun get(key: String): String?

    /** Freezes [body] for [key]. */
    fun put(key: String, body: String)

    /** Forgets [key]'s body, once its job is settled. */
    fun remove(key: String)

    /** The keys with a frozen body, to prune those of jobs that are gone ([Reconciler]). */
    fun keys(): Set<String>
}

/** [FrozenBodies] in app-private preferences (never backed up). */
internal class PreferenceFrozenBodies(context: Context) : FrozenBodies {
    private val preferences = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    override fun get(key: String): String? = preferences.getString(key, null)

    // Committed before the request goes, so a crash cannot lose it.
    override fun put(key: String, body: String) {
        preferences.edit().putString(key, body).commit()
    }

    override fun remove(key: String) {
        preferences.edit().remove(key).apply()
    }

    override fun keys(): Set<String> = preferences.all.keys.toSet()

    private companion object {
        const val FILE = "dev.passporter.notify.outbox"
    }
}
