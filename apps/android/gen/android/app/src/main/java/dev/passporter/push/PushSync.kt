package dev.passporter.push

import android.util.Log
import dev.passporter.device.DeviceAuthException
import java.io.IOException

/** Where push stands for this app after a [PushSync] step. */
sealed interface PushState {
    /** The app is not paired: nothing to subscribe, and no registration kept. */
    data object NotPaired : PushState

    /** Paired, but the distributor has not given an endpoint yet. */
    data object NoEndpoint : PushState

    /** The bridge holds this app's current registration: notices will arrive. */
    data object Subscribed : PushState

    /** The bridge has push turned off (404): `[push]` is not configured. */
    data object BridgeOff : PushState

    /**
     * The bridge refused the subscription (400): usually an endpoint not on
     * an allowed push service, so `[push] ntfy_host` must name the
     * distributor's server. [reason] is the bridge's one line.
     */
    data class Refused(
        /** Why, as the bridge said it. */
        val reason: String,
    ) : PushState

    /** The bridge holds as many subscriptions for this device as it allows (409). */
    data object Full : PushState

    /** Could not reach the bridge, or it failed: the next step tries again. */
    data object RetryLater : PushState
}

/** Whether to ask the distributor for an endpoint ([PushSync.readiness]). */
sealed interface Readiness {
    /** Paired, and the bridge has push on: register with [vapidKey]. */
    data class Register(
        /** The bridge's VAPID public key, for the distributor. */
        val vapidKey: String,
    ) : Readiness

    /** Not paired: nothing to register (any registration was dropped). */
    data object NotPaired : Readiness

    /** The bridge has push turned off. */
    data object BridgeOff : Readiness

    /** The pairing or the bridge could not be read: try again later. */
    data object Later : Readiness
}

/**
 * Keeps the bridge's copy of this app's push subscription in step with the
 * distributor's registration and the pairing (design §6.6). Every method
 * is blocking and must run on one worker thread at a time ([PushWork]).
 *
 * - A new endpoint is posted with `"payload": "notice"`; the endpoint it
 *   replaces is dropped first, if this device posted it.
 * - A new pairing (a new device id, perhaps on another bridge) starts
 *   afresh: the old registration is dropped at the distributor, which
 *   deletes its keys, so whatever the old pairing's bridge still sends can
 *   no longer be decrypted; the next registration has new keys.
 * - Unpaired, the registration is dropped the same way. The bridge's copy
 *   can no longer be removed from here (no session); revoking the old
 *   device on the bridge drops it.
 */
class PushSync(
    private val store: PushStore,
    private val bridge: PushBridge,
    /** Unregisters from the distributor (`UnifiedPush.unregister`), deleting the keys. */
    private val unregister: () -> Unit,
) {
    /** The bridge's VAPID key, once learnt, for the device it was learnt as. */
    @Volatile
    private var vapidKey: Pair<String, String>? = null

    /**
     * Whether the bridge confirmed the record's subscription in this
     * process. The first sync of a process posts it again (the bridge
     * answers 204 when it holds it), in case the bridge dropped it on its
     * own, after a 404 from a misconfigured proxy, say.
     */
    @Volatile
    private var confirmed = false

    /** The distributor gave [registration]: remember it and post it. */
    fun onNewEndpoint(registration: Registration): PushState {
        val record = store.load()
        if (record.registration != registration) {
            if (!save(record.copy(registration = registration))) return PushState.RetryLater
        }
        return sync()
    }

    /**
     * The distributor dropped this app's registration: removes the bridge's
     * copy while the app can still sign in, then forgets the registration.
     * When the pairing cannot be read just now, what the bridge holds is
     * kept in the record, for the next endpoint to replace.
     */
    fun onUnregistered(): PushState {
        val record = store.load()
        return when (val pairing = pairing()) {
            Pairing.Unknown -> {
                save(record.copy(registration = null))
                PushState.RetryLater
            }
            Pairing.Unpaired -> {
                save(PushRecord())
                PushState.NotPaired
            }
            is Pairing.Paired -> {
                val gone = record.subscribed?.takeIf { it.deviceId == pairing.deviceId }
                val left = record.copy(
                    registration = null,
                    subscribed = null,
                    stale = record.stale + listOfNotNull(gone?.registration?.endpoint),
                )
                save(dropStale(left))
                PushState.NoEndpoint
            }
        }
    }

    /**
     * Brings the bridge in step with the record: posts the registration if
     * the bridge does not hold it for this device yet; unpaired, or paired
     * anew, drops it.
     */
    fun sync(): PushState {
        val device = when (val pairing = pairing()) {
            is Pairing.Paired -> pairing.deviceId
            Pairing.Unpaired -> return unpaired()
            Pairing.Unknown -> return PushState.RetryLater
        }
        var record = store.load()
        if (startAfresh(record, device)) return PushState.NoEndpoint
        record.subscribed?.takeIf { it.registration.endpoint != record.registration?.endpoint }?.let {
            record = record.copy(subscribed = null, stale = record.stale + it.registration.endpoint)
        }
        // Before posting: the bridge may keep it even if its answer is lost.
        record = dropStale(record.copy(owner = device))
        if (!save(record)) return PushState.RetryLater
        val registration = record.registration ?: return PushState.NoEndpoint
        val current = Subscribed(registration, device)
        if (record.subscribed == current && confirmed) return PushState.Subscribed
        val response = try {
            bridge.subscribe(registration.subscribeJson())
        } catch (e: DeviceAuthException) {
            return failed(e)
        }
        val state = when (response.status) {
            201, 204 -> if (save(record.copy(subscribed = current))) {
                confirmed = true
                PushState.Subscribed
            } else {
                PushState.RetryLater
            }
            400 -> PushState.Refused(response.body.lineSequence().firstOrNull().orEmpty().take(MAX_REASON_CHARS))
            404 -> PushState.BridgeOff
            409 -> PushState.Full
            else -> PushState.RetryLater
        }
        if (state is PushState.Refused || state == PushState.BridgeOff || state == PushState.Full) {
            // Refused for sure: not one the bridge holds. (After a 5xx or a
            // lost answer it may be, so the record keeps what it had.)
            save(record.copy(subscribed = null))
        }
        Log.i(PUSH_TAG, "subscription at ${Registration.host(registration.endpoint)}: status ${response.status}, $state")
        return state
    }

    /**
     * Whether the app should register with the distributor now, and with
     * which VAPID key. Unpaired or paired anew, the old registration is
     * dropped first, as in [sync].
     */
    fun readiness(): Readiness {
        val device = when (val pairing = pairing()) {
            is Pairing.Paired -> pairing.deviceId
            Pairing.Unpaired -> {
                unpaired()
                return Readiness.NotPaired
            }
            Pairing.Unknown -> return Readiness.Later
        }
        val record = store.load()
        if (!startAfresh(record, device) && record.owner != device) save(record.copy(owner = device))
        vapidKey?.takeIf { it.first == device }?.let { return Readiness.Register(it.second) }
        return when (val key = bridge.vapidKey()) {
            is VapidKey.Known -> {
                vapidKey = device to key.value
                Readiness.Register(key.value)
            }
            VapidKey.PushOff -> Readiness.BridgeOff
            VapidKey.Unknown -> Readiness.Later
        }
    }

    /** Whether, and as which device, the app is paired. */
    private sealed interface Pairing {
        data class Paired(val deviceId: String) : Pairing
        data object Unpaired : Pairing

        /** The Keystore could not tell: try again later. */
        data object Unknown : Pairing
    }

    private fun pairing(): Pairing = try {
        bridge.deviceId()?.let(Pairing::Paired) ?: Pairing.Unpaired
    } catch (e: DeviceAuthException) {
        Log.w(PUSH_TAG, "could not read the pairing: ${e.code}")
        Pairing.Unknown
    }

    /**
     * If [record]'s keys belong to another device than [device] (the app
     * was paired anew), leaves the distributor so the keys are replaced;
     * whether it did.
     */
    private fun startAfresh(record: PushRecord, device: String): Boolean {
        val previous = record.owner ?: record.subscribed?.deviceId ?: return false
        if (previous == device) return false
        Log.i(PUSH_TAG, "paired anew: registering with new keys")
        reset()
        return true
    }

    /** Forgets everything and leaves the distributor, if there was anything. */
    private fun unpaired(): PushState {
        vapidKey = null
        if (store.load() != PushRecord()) {
            Log.i(PUSH_TAG, "not paired: unregistering from the distributor")
            reset()
        }
        return PushState.NotPaired
    }

    private fun reset() {
        vapidKey = null
        confirmed = false
        // First: if the process dies in between, the record still says to.
        unregister()
        save(PushRecord())
    }

    /** What a failed sign-in means: the pairing is gone, or try later. */
    private fun failed(e: DeviceAuthException): PushState = when (e) {
        is DeviceAuthException.NotPaired, is DeviceAuthException.Forgotten, is DeviceAuthException.NoBridge -> {
            // A revoked device's subscriptions went with it on the bridge.
            unpaired()
        }
        else -> {
            Log.w(PUSH_TAG, "could not reach the bridge: ${e.code}")
            PushState.RetryLater
        }
    }

    /**
     * Asks the bridge to drop each of [record]'s stale endpoints; the ones
     * it did not answer for (offline, a 5xx) stay for the next sync, so
     * none is left holding one of the device's subscription slots.
     */
    private fun dropStale(record: PushRecord): PushRecord {
        if (record.stale.isEmpty()) return record
        val left = record.stale.distinct().takeLast(MAX_STALE).filterNot(::dropped)
        return record.copy(stale = left)
    }

    /** Whether the bridge no longer holds [endpoint] for this device (204, or 404). */
    private fun dropped(endpoint: String): Boolean = try {
        val status = bridge.unsubscribe(Registration.unsubscribeJson(endpoint)).status
        Log.i(PUSH_TAG, "dropping subscription at ${Registration.host(endpoint)}: status $status")
        status == 204 || status == 404
    } catch (e: DeviceAuthException) {
        Log.w(PUSH_TAG, "could not drop a subscription: ${e.code}")
        false
    }

    private fun save(record: PushRecord): Boolean = try {
        store.save(record)
        true
    } catch (e: IOException) {
        Log.e(PUSH_TAG, "could not save the push record", e)
        false
    }

    private companion object {
        /** The bridge caps its own messages at 200 characters. */
        const val MAX_REASON_CHARS = 200

        /** The bridge keeps 8 subscriptions per device: never more to drop. */
        const val MAX_STALE = 8
    }
}
