package dev.passporter.device

import android.content.Context
import android.os.SystemClock
import java.io.File
import java.io.IOException
import java.security.ProviderException
import java.security.SignatureException
import java.util.EnumMap
import java.util.concurrent.CopyOnWriteArrayList

/** Whether this app is paired, and as which device. */
class DeviceStatus(val deviceId: DeviceId?) {
    /** True once pairing succeeded and the key is still there. */
    val paired: Boolean get() = deviceId != null
}

/**
 * Told when this app pairs or forgets its pairing ([DeviceAuth.addPairingListener]),
 * on the thread that did it, after the change.
 */
fun interface PairingListener {
    /** The pairing is now [status]. */
    fun onPairingChanged(status: DeviceStatus)
}

/**
 * A session token for one [scope], as the bridge granted it. `toString` is
 * redacted: tokens never go to logs.
 */
class SessionToken internal constructor(
    /** The bearer token itself. */
    val value: String,
    /** What it may do. */
    val scope: Scope,
    /** The device's name on the bridge, if the bridge reported one. */
    val deviceName: String?,
    /** When it expires, on the [DeviceAuth] clock (`SystemClock.elapsedRealtime`). */
    internal val expiresAtMs: Long,
) {
    /** The `Authorization` header value. */
    fun authorization(): String = "Bearer $value"

    override fun toString() = "SessionToken(<redacted>, $scope)"
}

/**
 * This app's device identity and its sessions with the bridge (design §8).
 *
 * The device key is a non-exportable P-256 key in the Android Keystore
 * ([KeystoreKeys]); the device id and the key's alias are in app-private,
 * never-backed-up storage. Everything here is blocking network or Keystore
 * work: call it from a worker thread, never the main thread. Calls are
 * serialized, so concurrent callers share one sign-in.
 *
 * ```kotlin
 * val auth = DeviceAuth.get(context)
 * val token = auth.token(Scope.CAR)              // cached until shortly before expiry
 * val reply = auth.send(Scope.APP, Method.POST, "/v1/agents/$id/reply", json)
 * ```
 */
class DeviceAuth internal constructor(
    private val origin: () -> BridgeOrigin?,
    private val store: DeviceStore,
    private val keys: DeviceKeys,
    private val transport: HttpTransport,
    private val now: () -> Long,
) {
    private val lock = Any()
    private val tokens = EnumMap<Scope, SessionToken>(Scope::class.java)
    private val listeners = CopyOnWriteArrayList<PairingListener>()

    /** Whether this app is paired with the saved bridge. */
    fun status(): DeviceStatus = synchronized(lock) { DeviceStatus(record()?.deviceId) }

    /**
     * Calls [listener] after every [pair] that succeeds and every [forget]
     * (not when the bridge revokes the device: that surfaces as
     * [DeviceAuthException.Forgotten] to whoever asked for a token).
     */
    fun addPairingListener(listener: PairingListener) {
        listeners.add(listener)
    }

    /** Stops calling [listener]. */
    fun removePairingListener(listener: PairingListener) {
        listeners.remove(listener)
    }

    /**
     * Pairs with [code] (from `passporter pair`): makes a new key, registers
     * it, and only then replaces any earlier pairing. On failure the earlier
     * pairing, if any, is kept.
     */
    fun pair(code: PairingCode, name: DeviceName?): DeviceId {
        val id = pairLocked(code, name)
        changed(DeviceStatus(id))
        return id
    }

    private fun pairLocked(code: PairingCode, name: DeviceName?): DeviceId = synchronized(lock) {
        val bridge = origin() ?: throw DeviceAuthException.NoBridge()
        val new = keystore { keys.create() }
        val paired = try {
            BridgeClient(bridge, transport).pair(PairRequest(code, new.publicKey, name))
        } catch (e: Exception) {
            discard(new.alias)
            throw e
        }
        val previous = store.load()
        try {
            store.save(DeviceRecord(paired.deviceId, new.alias, bridge))
        } catch (e: IOException) {
            // The bridge now lists a device this app cannot use: it holds a
            // slot until it is revoked there. Rare (storage full).
            discard(new.alias)
            throw DeviceAuthException.Storage(e)
        }
        if (previous != null && previous.keyAlias != new.alias) discard(previous.keyAlias)
        tokens.clear()
        paired.deviceId
    }

    /**
     * A token for [scope]: the cached one while it has more than
     * [REFRESH_MARGIN_MS] left, else a fresh one. If the bridge then refuses
     * it (401, after a bridge restart say), pass it to [invalidate] and ask
     * again; [send] does that for you.
     *
     * @throws DeviceAuthException [DeviceAuthException.Forgotten] when the
     *   bridge no longer knows this device (the pairing is then forgotten
     *   here as well), or any other reason it failed.
     */
    fun token(scope: Scope): SessionToken = synchronized(lock) {
        tokens[scope]?.takeIf { it.expiresAtMs - now() > REFRESH_MARGIN_MS } ?: mint(scope)
    }

    /** A token for [scope] made now, never the cached one (which it replaces). */
    fun freshToken(scope: Scope): SessionToken = synchronized(lock) { mint(scope) }

    /** Drops [rejected] from the cache, if it is still the cached token for its scope. */
    fun invalidate(rejected: SessionToken) = synchronized(lock) {
        if (tokens[rejected.scope] === rejected) tokens.remove(rejected.scope)
    }

    /** Seconds [token] has left, for callers that schedule a renewal. */
    fun secondsLeft(token: SessionToken): Long = ((token.expiresAtMs - now()) / 1000).coerceAtLeast(0)

    /** Forgets the pairing: deletes the key, the record and every cached token. */
    fun forget() {
        synchronized(lock) {
            store.load()?.let { discard(it.keyAlias) }
            store.clear()
            tokens.clear()
        }
        changed(DeviceStatus(null))
    }

    /** Tells the listeners, outside the lock (they may call back in). */
    private fun changed(status: DeviceStatus) {
        listeners.forEach { it.onPairingChanged(status) }
    }

    /**
     * An authenticated request to the bridge with a [scope] token: [path] is
     * absolute (`/v1/...`), [jsonBody] goes as `application/json` (only with
     * [Method.POST] or [Method.PUT]). A 401 is
     * retried once with a fresh token. Any answer the bridge gives is
     * returned as is; only failing to get a token throws.
     *
     * @throws DeviceAuthException as [token], [DeviceAuthException.Unreachable],
     *   or [DeviceAuthException.InvalidInput] when [path] is not absolute.
     */
    fun send(scope: Scope, method: Method, path: String, jsonBody: String? = null): HttpResponse {
        if (jsonBody != null && !method.hasBody) throw DeviceAuthException.InvalidInput("body for ${method.name}")
        val bridge = origin() ?: throw DeviceAuthException.NoBridge()
        val url = try {
            bridge.resolve(path)
        } catch (e: IllegalArgumentException) {
            throw DeviceAuthException.InvalidInput("path")
        }
        val first = token(scope)
        val response = sendWith(first, method, url, jsonBody)
        if (response.status != 401) return response
        invalidate(first)
        return sendWith(token(scope), method, url, jsonBody)
    }

    private fun sendWith(token: SessionToken, method: Method, url: String, jsonBody: String?): HttpResponse {
        val headers = buildMap {
            put("Authorization", token.authorization())
            put("Accept", "application/json")
            if (jsonBody != null) put("Content-Type", "application/json")
        }
        return try {
            transport.send(HttpRequest(method.name, url, headers, jsonBody))
        } catch (e: IOException) {
            throw DeviceAuthException.Unreachable(e)
        }
    }

    /** The record, if it belongs to the saved bridge and its key still exists. Holds [lock]. */
    private fun record(): DeviceRecord? {
        val record = store.load() ?: return null
        if (record.origin != origin()) return null
        if (!keystore { keys.exists(record.keyAlias) }) {
            // The key is gone (the Keystore was reset): the pairing is useless.
            store.clear()
            return null
        }
        return record
    }

    /** Signs in: challenge, signature, session. One retry on a 401 with a fresh challenge. Holds [lock]. */
    private fun mint(scope: Scope): SessionToken {
        val record = record() ?: throw DeviceAuthException.NotPaired()
        val client = BridgeClient(record.origin, transport)
        try {
            return try {
                mintOnce(client, record, scope)
            } catch (e: DeviceAuthException.Refused) {
                // Usually a challenge lost to a bridge restart mid-sign-in.
                mintOnce(client, record, scope)
            }
        } catch (e: DeviceAuthException.Forgotten) {
            discard(record.keyAlias)
            store.clear()
            tokens.clear()
            throw e
        }
    }

    private fun mintOnce(client: BridgeClient, record: DeviceRecord, scope: Scope): SessionToken {
        val asked = now()
        val challenge = client.challenge(ChallengeRequest(record.deviceId)).challenge
        val message = P256.signedMessage(record.deviceId, challenge)
        val signature = sign(record.keyAlias, message)
        val grant = client.session(SessionRequest(record.deviceId, challenge, signature, scope))
        if (grant.scope != scope) throw DeviceAuthException.BadResponse("granted ${grant.scope.wire}, asked ${scope.wire}")
        // Counted from before the request, so the cache never outlives the token.
        val lifetimeMs = grant.expiresInSecs.coerceAtMost(MAX_LIFETIME_SECS) * 1000
        val token = SessionToken(grant.token, scope, grant.deviceName, asked + lifetimeMs)
        tokens[scope] = token
        return token
    }

    /** Deletes the key [alias], best effort: a key left behind is unused, not a risk. */
    private fun discard(alias: String) {
        runCatching { keys.delete(alias) }
    }

    /**
     * Signs with the Keystore, trying once more after a moment: a busy
     * Keystore (just after boot, say) usually answers the second time. A
     * failure after that is reported as final, so callers do not loop on a
     * key that will never work.
     */
    private fun sign(alias: String, message: ByteArray): ByteArray = try {
        keys.sign(alias, message)
    } catch (e: Exception) {
        // Only what a busy Keystore throws; a missing key or a malformed
        // signature will not get better.
        if (e !is ProviderException && e !is SignatureException) throw DeviceAuthException.Keystore(e)
        Thread.sleep(SIGN_RETRY_MS)
        keystore { keys.sign(alias, message) }
    }

    /** Runs Keystore work, reporting its failures as [DeviceAuthException.Keystore]. */
    private inline fun <T> keystore(work: () -> T): T = try {
        work()
    } catch (e: DeviceAuthException) {
        throw e
    } catch (e: Exception) {
        throw DeviceAuthException.Keystore(e)
    }

    companion object {
        /**
         * A cached token is reused only while it has more than this left.
         * Longer than the PWA's own renewal margin (one minute), so a token
         * handed out is never one the PWA would renew at once.
         */
        const val REFRESH_MARGIN_MS = 120_000L

        /** Longer than any session the bridge grants (15 min); keeps the arithmetic sane. */
        private const val MAX_LIFETIME_SECS = 86_400L

        /** The pause before the one retry of a failed signature. */
        private const val SIGN_RETRY_MS = 1_000L

        /** The record's file in `noBackupFilesDir`. */
        private const val RECORD_FILE = "passporter-device.json"

        @Volatile
        private var instance: DeviceAuth? = null

        /** The app's one [DeviceAuth]. Any context will do; the application context is kept. */
        fun get(context: Context): DeviceAuth = instance ?: synchronized(this) {
            instance ?: context.applicationContext.let { app ->
                DeviceAuth(
                    origin = { BridgeOrigin.saved(app.dataDir) },
                    store = DeviceStore(File(app.noBackupFilesDir, RECORD_FILE)),
                    keys = KeystoreKeys(),
                    transport = UrlConnectionTransport(),
                    now = SystemClock::elapsedRealtime,
                )
            }.also { instance = it }
        }
    }
}
