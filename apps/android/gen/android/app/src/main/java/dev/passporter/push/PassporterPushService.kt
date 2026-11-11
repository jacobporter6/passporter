package dev.passporter.push

import android.util.Log
import dev.passporter.notify.NoticeNotifier
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
 * and shown by [NoticeNotifier]; endpoints and unregistrations go to
 * [PushSync]. Everything runs on [PushWork]'s thread.
 */
class PassporterPushService : PushService() {
    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        if (instance != PushWork.INSTANCE) return
        val keys = endpoint.pubKeySet
        val registration = keys?.let { Registration.of(endpoint.url, it.pubKey, it.auth) }
        if (registration == null) {
            Log.w(TAG, "the distributor gave an endpoint without usable Web Push keys; ignored")
            return
        }
        val app = applicationContext
        PushWork.run {
            val state = PushWork.sync(app).onNewEndpoint(registration)
            Log.i(TAG, "new endpoint at ${Registration.host(registration.endpoint)}: $state")
        }
    }

    override fun onMessage(message: PushMessage, instance: String) {
        if (instance != PushWork.INSTANCE) return
        val app = applicationContext
        PushWork.run {
            val incoming = IncomingPush(
                show = NoticeNotifier(app)::show,
                paired = PushWork.sync(app)::checkPaired,
            ).handle(message)
            // Never the content: it is agent output.
            Log.i(TAG, "push message: $incoming")
        }
    }

    override fun onUnregistered(instance: String) {
        if (instance != PushWork.INSTANCE) return
        val app = applicationContext
        PushWork.run {
            val state = PushWork.sync(app).onUnregistered()
            Log.i(TAG, "unregistered by the distributor: $state")
        }
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        // The next time the app opens tries again (PushRegistration).
        Log.w(TAG, "the distributor refused to register: $reason")
    }

    private companion object {
        const val TAG = "PassporterPush"
    }
}
