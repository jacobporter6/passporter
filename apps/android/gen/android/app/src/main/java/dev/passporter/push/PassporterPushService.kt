package dev.passporter.push

import android.util.Log
import dev.passporter.device.DeviceId
import dev.passporter.notify.NoticeNotifier
import dev.passporter.notify.actions.pairedDevice
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage

/**
 * Receives UnifiedPush events from the distributor (design §6.6). The
 * connector starts it when one arrives, also with the app's process cold;
 * it is not exported (only the connector's receiver, which checks each
 * event's registration token, reaches it).
 *
 * Messages are decrypted by the connector with this app's Web Push keys
 * and shown by [NoticeNotifier] right in the callback: a few milliseconds
 * of local work, never queued behind a network call. With the service
 * bound that is on the connector's receiver thread, under its wake lock;
 * from cold, the connector binds the service first and delivers the event
 * on the main thread as soon as it is up.
 * Endpoints and unregistrations go to [PushSync] on [PushWork]'s thread.
 */
class PassporterPushService : PushService() {
    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        if (instance != PushWork.INSTANCE) return
        // A temporary endpoint (a fallback distributor's, while the primary
        // is down) is posted like any other: the bridge refuses it unless its
        // host is allowed, and the primary's comes back as a new endpoint.
        val keys = endpoint.pubKeySet
        val registration = keys?.let { Registration.of(endpoint.url, it.pubKey, it.auth) }
        if (registration == null) {
            Log.w(PUSH_TAG, "the distributor gave an endpoint the bridge could not use (not https, or no Web Push keys); ignored")
            return
        }
        val app = applicationContext
        PushWork.run {
            val state = PushWork.sync(app).onNewEndpoint(registration)
            Log.i(PUSH_TAG, "new endpoint at ${Registration.host(registration.endpoint)}: $state")
        }
    }

    override fun onMessage(message: PushMessage, instance: String) {
        if (instance != PushWork.INSTANCE) return
        val store = PushWork.store(this)
        // The keys that decrypted it were registered for the owner: the
        // notices' actions answer as that device, and only while it is paired.
        val owner = { store.load().owner?.let(DeviceId::parse) }
        val incoming = IncomingPush(
            show = NoticeNotifier(this, owner)::show,
            // Keys of an older pairing (not yet replaced after a new one): its
            // notices must not come back into the shade the pairing change cleared.
            registered = {
                val record = store.load()
                record.registration != null && record.owner != null && record.owner == pairedDevice(this)?.value
            },
        ).handle(message)
        // Never the content: it is agent output.
        Log.i(PUSH_TAG, "push message: $incoming")
    }

    override fun onUnregistered(instance: String) {
        if (instance != PushWork.INSTANCE) return
        val app = applicationContext
        PushWork.run {
            val state = PushWork.sync(app).onUnregistered()
            Log.i(PUSH_TAG, "unregistered by the distributor: $state")
        }
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        // The next time the app opens tries again (PushRegistration).
        Log.w(PUSH_TAG, "the distributor refused to register: $reason")
    }
}
