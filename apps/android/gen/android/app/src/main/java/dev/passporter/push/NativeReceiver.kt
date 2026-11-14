package dev.passporter.push

import android.util.Log
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId
import dev.passporter.notify.NoticeOutcome
import dev.passporter.notify.actions.PairedAs
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.security.GeneralSecurityException
import java.util.UUID
import org.json.JSONException
import org.json.JSONObject
import org.unifiedpush.android.connector.keys.KeyManager

/** Where the built-in transport stands ([NativeReceiver]). */
sealed interface NativeState {
    /** Not paired: nothing registered, no keys kept. */
    data object NotPaired : NativeState

    /** You chose the external distributor: the built-in transport is off. */
    data object Off : NativeState

    /** The bridge has no companion (`POST /v1/push/native` answers 404). */
    data object BridgeHasNone : NativeState

    /** The bridge holds this app's registration; the stream is not open. */
    data object Registered : NativeState

    /** The stream is open: news arrives. */
    data object Connected : NativeState

    /** The bridge refused the registration ([status]: 400, 403 or 409). */
    data class Refused(
        /** The HTTP status. */
        val status: Int,
    ) : NativeState

    /** Could not reach the bridge, or it failed: tried again later. */
    data object RetryLater : NativeState
}

/** What became of one stream line ([NativeReceiver.ingest]). */
sealed interface Ingested {
    /** Not a message (ntfy's `open` and `keepalive` events): nothing to do. */
    data object Ignored : Ingested

    /** The envelope was refused unread. */
    data class Refused(
        /** Why. */
        val reason: EnvelopeRefusal,
    ) : Ingested

    /** Verified, but the receiver's keys could not decrypt it, or it was not text. */
    data object Undecryptable : Ingested

    /** The pairing changed since registering: dropped, as [IncomingPush] drops it. */
    data object OtherPairing : Ingested

    /** Shown by the notifier, which did [outcome]. */
    data class Shown(
        /** What the notifier did. */
        val outcome: NoticeOutcome,
    ) : Ingested
}

/**
 * The built-in transport's receiver (design §6.6): registers this pairing
 * with the bridge's companion and reads its stream of signed envelopes,
 * turning each into notifications through the same path as a push from
 * an external distributor. Every method is blocking: [register], [leave]
 * and [reset] run on [PushWork]'s thread, [listen] on the
 * [NativePushService]'s.
 *
 * - The receiver's Web Push keys are the connector's key manager's, under
 *   their own instance ([KEY_INSTANCE]), apart from the external
 *   registration's.
 * - The registration belongs to the device it was made for (its owner). A
 *   new pairing, or none, drops the keys and the registration, so what the
 *   old pairing's bridge still sends can no longer be decrypted.
 * - An envelope is shown only if it verifies with the key the bridge
 *   named at registration, is for this registration, has not expired, and
 *   decrypts with this app's keys; and only while its owner is still the
 *   pairing ([paired]: not paired counts as not the owner, as for
 *   [IncomingPush]). Its notices' actions answer as the owner.
 * - Several envelopes in a row that fail the signature mean the bridge's
 *   key changed (a new VAPID key): it registers again for the new key.
 * - The stream's cursor advances after a message is shown, so a crash in
 *   between shows it once more at worst; the notifier drops a ping it has
 *   shown.
 */
class NativeReceiver(
    private val store: NativeStore,
    private val bridge: NativeBridge,
    private val keys: KeyManager,
    /** Shows a decrypted payload as [DeviceId]'s (`NoticeNotifier.show`). */
    private val show: (DeviceId, String) -> NoticeOutcome,
    /** What the app is paired as now, cheaply ([dev.passporter.device.DeviceAuth.savedDevice]). */
    private val paired: () -> PairedAs,
    /** Unix seconds now. */
    private val nowSecs: () -> Long = { System.currentTimeMillis() / 1000 },
    /** A fresh registration id. */
    private val newId: () -> String = { UUID.randomUUID().toString() },
) {
    /** Serializes changes of the record and the keys ([register] runs on two threads). */
    private val lock = Any()

    /** Whether the bridge confirmed the registration in this process: registered once per process. */
    @Volatile
    private var confirmed = false

    /** Envelopes in a row that failed the signature ([MAX_FORGED]). */
    @Volatile
    private var forged = 0

    /** The record (safe from any thread). */
    fun record(): NativeRecord = store.load()

    /**
     * Registers this pairing with the bridge's companion, unless you chose
     * the external distributor ([NativeState.Off], which also leaves) or
     * the app is not paired.
     */
    fun register(): NativeState = synchronized(lock) { registerLocked() }

    private fun registerLocked(): NativeState {
        val device = try {
            bridge.deviceId()
        } catch (e: DeviceAuthException) {
            return NativeState.RetryLater
        } ?: return notPaired()
        var record = store.load()
        if (record.choice == Transport.EXTERNAL) return leave()
        if (record.owner != null && record.owner != device.value) record = reset()
        if (record.registered && confirmed) return NativeState.Registered
        val public = try {
            if (!keys.exists(KEY_INSTANCE)) keys.generate(KEY_INSTANCE)
            keys.getPublicKeySet(KEY_INSTANCE)
        } catch (e: GeneralSecurityException) {
            Log.e(PUSH_TAG, "could not make the built-in receiver's keys", e)
            null
        } ?: return NativeState.RetryLater
        val id = record.registrationId ?: newId()
        // Saved before posting: the bridge may keep it even if its answer is lost.
        record = record.copy(owner = device.value, registrationId = id)
        if (!save(record)) return NativeState.RetryLater
        val body = JSONObject()
            .put("registrationId", id)
            .put("p256dh", public.pubKey)
            .put("auth", public.auth)
            .toString()
        val response = try {
            bridge.register(body)
        } catch (e: DeviceAuthException) {
            return failed(e)
        }
        val state = when (response.status) {
            200 -> registered(record, response.body)
            404 -> {
                save(record.copy(signingKey = null))
                NativeState.BridgeHasNone
            }
            // 409: the device holds its 8 subscriptions (an id taken by another device is
            // never seen in practice: ids are random). The id is kept for the next try.
            400, 403, 409 -> NativeState.Refused(response.status)
            else -> NativeState.RetryLater
        }
        Log.i(PUSH_TAG, "built-in registration: status ${response.status}, $state")
        return state
    }

    /** Reads a 200 answer to the registration: its signing key, once checked. */
    private fun registered(record: NativeRecord, body: String): NativeState {
        val answer = try {
            JSONObject(body)
        } catch (e: JSONException) {
            return NativeState.RetryLater
        }
        val key = answer.optString("signingKey")
        val expected = streamPath(record.registrationId ?: return NativeState.RetryLater)
        if (answer.optInt("version") != 1 || !NativeEnvelope.isSigningKey(key) || answer.optString("streamPath") != expected) {
            Log.w(PUSH_TAG, "the bridge's built-in registration answer was not understood")
            return NativeState.RetryLater
        }
        if (!save(record.copy(signingKey = key))) return NativeState.RetryLater
        confirmed = true
        forged = 0
        return NativeState.Registered
    }

    /**
     * Turns the built-in transport off for the external distributor: asks
     * the bridge to drop the registration (best effort: unread envelopes
     * expire within the hour, and revoking the device drops it too), then
     * forgets it and the keys.
     */
    fun leave(): NativeState = synchronized(lock) {
        val record = store.load()
        val owner = record.owner?.let(DeviceId::parse)
        val id = record.registrationId
        if (owner != null && id != null) {
            val endpoint = JSONObject().put("endpoint", RESERVED_PREFIX + id).toString()
            try {
                val status = bridge.unsubscribe(endpoint, owner).status
                Log.i(PUSH_TAG, "dropping the built-in registration: status $status")
            } catch (e: DeviceAuthException) {
                Log.w(PUSH_TAG, "could not drop the built-in registration: ${e.code}")
            }
        }
        reset()
        NativeState.Off
    }

    /** Forgets the registration and deletes the keys, keeping your choice; the record now. */
    fun reset(): NativeRecord = synchronized(lock) {
        confirmed = false
        try {
            if (keys.exists(KEY_INSTANCE)) keys.delete(KEY_INSTANCE)
        } catch (e: GeneralSecurityException) {
            Log.e(PUSH_TAG, "could not delete the built-in receiver's keys", e)
        }
        val left = store.load().forgotten()
        save(left)
        left
    }

    /** Sets your [choice]; the next [register] acts on it. */
    fun choose(choice: Transport) {
        synchronized(lock) { save(store.load().copy(choice = choice)) }
    }

    private fun notPaired(): NativeState {
        if (store.load().owner != null) reset()
        return NativeState.NotPaired
    }

    private fun failed(e: DeviceAuthException): NativeState = when (e) {
        is DeviceAuthException.NotPaired, is DeviceAuthException.Forgotten, is DeviceAuthException.NoBridge -> notPaired()
        else -> {
            Log.w(PUSH_TAG, "could not reach the bridge: ${e.code}")
            NativeState.RetryLater
        }
    }

    /**
     * Reads the stream once, until it ends, it fails, or [running] turns
     * false (closing what [opened] was given also ends it, from another
     * thread). [connected] is called once the stream is open; each line is
     * handled inside [guard] (a wake lock). Answers why
     * it ended: [NativeState.RetryLater] when worth reconnecting, else what
     * stops the service. A registration the bridge no longer holds (404)
     * is made afresh first.
     */
    fun listen(
        running: () -> Boolean,
        opened: (NativeStream) -> Unit,
        connected: () -> Unit,
        guard: (() -> Ingested) -> Ingested = { it() },
    ): NativeState {
        val record = store.load()
        if (record.choice == Transport.EXTERNAL) return NativeState.Off
        val owner = record.owner?.let(DeviceId::parse)
        val id = record.registrationId
        if (!record.registered || owner == null || id == null) return NativeState.NotPaired
        val path = streamPath(id) + "?since=" + (record.cursor ?: "all")
        val stream = try {
            bridge.openStream(path, owner)
        } catch (e: DeviceAuthException) {
            return failed(e)
        } catch (e: IOException) {
            return NativeState.RetryLater
        }
        stream.use {
            opened(stream)
            when (stream.status) {
                200 -> Unit
                400 -> {
                    // The cursor is no longer one the companion knows: read all it holds.
                    synchronized(lock) { save(store.load().copy(cursor = null)) }
                    return NativeState.RetryLater
                }
                404 -> {
                    synchronized(lock) {
                        confirmed = false
                        save(store.load().copy(signingKey = null))
                    }
                    return register()
                }
                else -> return NativeState.RetryLater
            }
            connected()
            try {
                while (running()) {
                    val line = stream.nextLine() ?: break
                    val outcome = guard { ingest(line) }
                    if (outcome !is Ingested.Ignored) Log.i(PUSH_TAG, "built-in message: $outcome")
                    if (forged >= MAX_FORGED) {
                        Log.w(PUSH_TAG, "envelopes keep failing the signature: registering again")
                        confirmed = false
                        return register()
                    }
                }
            } catch (e: IOException) {
                // A dropped connection or a read timeout: reconnect. Never logged with content.
            }
        }
        return NativeState.RetryLater
    }

    /** Handles one stream line. Never logs its content. */
    fun ingest(line: String): Ingested {
        val event = try {
            JSONObject(line)
        } catch (e: JSONException) {
            return Ingested.Ignored
        }
        if (event.optString("event") != "message") return Ingested.Ignored
        val id = event.optString("id").takeIf(::isCursor) ?: return Ingested.Ignored
        val record = store.load()
        val owner = record.owner?.let(DeviceId::parse)
        val registration = record.registrationId
        val key = record.signingKey
        if (owner == null || registration == null || key == null) return Ingested.OtherPairing
        val opened = NativeEnvelope.open(event.optString("message"), registration, key, nowSecs())
        forged = if (opened == Opened.Refused(EnvelopeRefusal.FORGED)) forged + 1 else 0
        val outcome = when (opened) {
            is Opened.Refused -> Ingested.Refused(opened.reason)
            is Opened.Ciphertext -> deliver(opened.body, owner)
        }
        // Read past it either way: a refused message stays refused.
        synchronized(lock) {
            if (store.load().registrationId == registration) save(store.load().copy(cursor = id))
        }
        return outcome
    }

    private fun deliver(body: ByteArray, owner: DeviceId): Ingested {
        val plaintext = try {
            keys.decrypt(KEY_INSTANCE, body)
        } catch (e: GeneralSecurityException) {
            null
        } ?: return Ingested.Undecryptable
        val text = utf8(plaintext) ?: return Ingested.Undecryptable
        // Unpaired, or paired as another device: dropped, as IncomingPush drops it.
        if (paired().isNot(owner)) return Ingested.OtherPairing
        return Ingested.Shown(show(owner, text))
    }

    private fun save(record: NativeRecord): Boolean = try {
        store.save(record)
        true
    } catch (e: IOException) {
        Log.e(PUSH_TAG, "could not save the built-in record", e)
        false
    }

    private fun utf8(bytes: ByteArray): String? = try {
        Charsets.UTF_8.newDecoder()
            .onMalformedInput(CodingErrorAction.REPORT)
            .onUnmappableCharacter(CodingErrorAction.REPORT)
            .decode(ByteBuffer.wrap(bytes))
            .toString()
    } catch (e: CharacterCodingException) {
        null
    }

    companion object {
        /** Envelopes in a row failing the signature before registering again. */
        const val MAX_FORGED = 3

        /** The key manager instance of the built-in receiver's keys. */
        const val KEY_INSTANCE = "passporter-builtin"

        /** The bridge's reserved endpoint prefix (`push::native`). */
        private const val RESERVED_PREFIX = "https://native.passporter.invalid/"

        /** The stream of registration [id]. */
        fun streamPath(id: String): String = "/v1/push/native/$id/stream"

        /** Whether [value] is a stream cursor the bridge accepts (ntfy's message ids). */
        fun isCursor(value: String): Boolean =
            value.length in 1..64 && value.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' || it == '-' || it == '_' }
    }
}
