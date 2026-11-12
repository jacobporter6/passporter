package dev.passporter.push

import dev.passporter.device.DeviceId
import dev.passporter.notify.NoticeOutcome
import dev.passporter.notify.actions.PairedAs
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import org.unifiedpush.android.connector.data.PushMessage

/** What became of one push message ([IncomingPush.handle]). */
sealed interface Incoming {
    /** Handed to the notifier, which did [outcome]. */
    data class Shown(
        /** What the notifier did. */
        val outcome: NoticeOutcome,
    ) : Incoming

    /**
     * The connector could not decrypt it with this app's keys: not from
     * the bridge (or for keys since replaced). Dropped unseen, so no one
     * but the bridge can make the app show a notice.
     */
    data object NotDecrypted : Incoming

    /** Decrypted, but not UTF-8 text: dropped. */
    data object NotText : Incoming

    /**
     * The app holds no registration for the device it is paired as now
     * (it was unpaired, or paired anew and the keys are not replaced yet): dropped.
     */
    data object NotRegistered : Incoming
}

/**
 * Turns a push message from the distributor into notifications: only one
 * the connector decrypted with this app's Web Push keys (RFC 8291), and
 * only while the app holds a registration for the device it is paired as
 * now (unpairing and a new pairing drop it). Everything
 * it needs travels in the message, so it works under Doze, with no network,
 * and it takes no lock a slow sign-in could hold.
 */
class IncomingPush(
    /** Shows a decrypted payload (`NoticeNotifier.show`). */
    private val show: (String) -> NoticeOutcome,
    /** Whether the app holds a registration ([PushStore]) made for the device it is paired as now. */
    private val registered: () -> Boolean,
) {
    /** Shows [message], or says why not. Never logs or returns its content. */
    fun handle(message: PushMessage): Incoming {
        if (!message.decrypted) return Incoming.NotDecrypted
        val text = utf8(message.content) ?: return Incoming.NotText
        if (!registered()) return Incoming.NotRegistered
        return Incoming.Shown(show(text))
    }

    companion object {
        /**
         * Whether [record] holds a registration of the pairing the app has
         * now ([paired]). Keys of an older one, not yet replaced after a new
         * pairing, are not: their notices must not come back into the shade
         * the pairing change cleared. When the app cannot tell (a busy
         * Keystore after boot), a registration counts.
         */
        fun isCurrent(record: PushRecord, paired: PairedAs): Boolean =
            record.registration != null && !paired.isNot(record.owner?.let(DeviceId::parse))
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
}
