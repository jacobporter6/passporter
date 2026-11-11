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
 * - Re-pairing (a new device id) posts the registration again, as the new
 *   device.
 * - Unpaired, the registration is dropped and the distributor told
 *   ([unregister]), so pushes stop reaching the phone. The bridge's copy
 *   can no longer be removed from here (no session); revoking the old
 *   device on the bridge drops it. Until then what the bridge sends goes
 *   nowhere: the distributor dropped the registration, and its keys are
 *   gone.
 */
class PushSync(
    private val store: PushStore,
    private val bridge: PushBridge,
    /** Unregisters from the distributor (`UnifiedPush.unregister`). */
    private val unregister: () -> Unit,
) {
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
     * copy while the app can still sign in, then forgets both.
     */
    fun onUnregistered(): PushState {
        val record = store.load()
        val device = (pairing() as? Pairing.Paired)?.deviceId
        record.subscribed?.takeIf { device != null && it.deviceId == device }?.let { drop(it) }
        save(PushRecord())
        return if (device == null) PushState.NotPaired else PushState.NoEndpoint
    }

    /**
     * Brings the bridge in step with the record: posts the registration if
     * the bridge does not hold it for this device yet; unpaired, drops it.
     */
    fun sync(): PushState {
        val device = when (val pairing = pairing()) {
            is Pairing.Paired -> pairing.deviceId
            Pairing.Unpaired -> return unpaired()
            Pairing.Unknown -> return PushState.RetryLater
        }
        val record = store.load()
        val registration = record.registration ?: return PushState.NoEndpoint
        val current = Subscribed(registration, device)
        if (record.subscribed == current) return PushState.Subscribed
        record.subscribed
            ?.takeIf { it.deviceId == device && it.registration.endpoint != registration.endpoint }
            ?.let { drop(it) }
        val response = try {
            bridge.subscribe(registration.subscribeJson())
        } catch (e: DeviceAuthException) {
            return failed(e)
        }
        val state = when (response.status) {
            201, 204 -> if (save(PushRecord(registration, current))) PushState.Subscribed else PushState.RetryLater
            400 -> PushState.Refused(response.body.lineSequence().firstOrNull().orEmpty().take(MAX_REASON_CHARS))
            404 -> PushState.BridgeOff
            409 -> PushState.Full
            else -> PushState.RetryLater
        }
        if (state != PushState.Subscribed) {
            // A subscription that was not kept must not be taken for one.
            save(PushRecord(registration, null))
        }
        Log.i(TAG, "subscription at ${Registration.host(registration.endpoint)}: status ${response.status}, $state")
        return state
    }

    /**
     * Whether the app should register with the distributor now, and with
     * which VAPID key. Unpaired, the registration is dropped as in [sync].
     */
    fun readiness(): Readiness = when (val pairing = pairing()) {
        Pairing.Unpaired -> {
            unpaired()
            Readiness.NotPaired
        }
        Pairing.Unknown -> Readiness.Later
        is Pairing.Paired -> when (val key = bridge.vapidKey()) {
            is VapidKey.Known -> Readiness.Register(key.value)
            VapidKey.PushOff -> Readiness.BridgeOff
            VapidKey.Unknown -> Readiness.Later
        }
    }

    /**
     * Whether the app is paired, before a message is shown. Unpaired, the
     * registration is dropped as in [sync]. Null when the Keystore could
     * not tell.
     */
    fun checkPaired(): Boolean? = when (pairing()) {
        is Pairing.Paired -> true
        Pairing.Unpaired -> {
            unpaired()
            false
        }
        Pairing.Unknown -> null
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
        Log.w(TAG, "could not read the pairing: ${e.code}")
        Pairing.Unknown
    }

    /** Forgets everything and leaves the distributor, if there was anything. */
    private fun unpaired(): PushState {
        val record = store.load()
        if (record != PushRecord()) {
            Log.i(TAG, "not paired: unregistering from the distributor")
            save(PushRecord())
            unregister()
        }
        return PushState.NotPaired
    }

    /** What a failed sign-in means: the pairing is gone, or try later. */
    private fun failed(e: DeviceAuthException): PushState = when (e) {
        is DeviceAuthException.NotPaired, is DeviceAuthException.Forgotten, is DeviceAuthException.NoBridge -> {
            // A revoked device's subscriptions went with it on the bridge.
            unpaired()
        }
        else -> {
            Log.w(TAG, "could not reach the bridge: ${e.code}")
            PushState.RetryLater
        }
    }

    /** Asks the bridge to drop [subscribed], best effort: it drops dead endpoints itself too. */
    private fun drop(subscribed: Subscribed) {
        try {
            val status = bridge.unsubscribe(Registration.unsubscribeJson(subscribed.registration.endpoint)).status
            Log.i(TAG, "dropped subscription at ${Registration.host(subscribed.registration.endpoint)}: status $status")
        } catch (e: DeviceAuthException) {
            Log.w(TAG, "could not drop a subscription: ${e.code}")
        }
    }

    private fun save(record: PushRecord): Boolean = try {
        store.save(record)
        true
    } catch (e: IOException) {
        Log.e(TAG, "could not save the push record", e)
        false
    }

    private companion object {
        const val TAG = "PassporterPush"

        /** The bridge caps its own messages at 200 characters. */
        const val MAX_REASON_CHARS = 200
    }
}
