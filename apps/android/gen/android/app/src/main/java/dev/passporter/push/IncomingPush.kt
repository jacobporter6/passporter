package dev.passporter.push

import dev.passporter.notify.NoticeOutcome
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

    /** The app is not paired (any more): dropped. */
    data object NotPaired : Incoming
}

/**
 * Turns a push message from the distributor into notifications: only one
 * the connector decrypted with this app's Web Push keys (RFC 8291), and
 * only while paired. Everything it needs travels in the message, so it
 * works under Doze, with no network.
 */
class IncomingPush(
    /** Shows a decrypted payload (`NoticeNotifier.show`). */
    private val show: (String) -> NoticeOutcome,
    /** Whether the app is paired; null when that is unknown (shown then). */
    private val paired: () -> Boolean?,
) {
    /** Shows [message], or says why not. Never logs or returns its content. */
    fun handle(message: PushMessage): Incoming {
        if (!message.decrypted) return Incoming.NotDecrypted
        val text = utf8(message.content) ?: return Incoming.NotText
        if (paired() == false) return Incoming.NotPaired
        return Incoming.Shown(show(text))
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
