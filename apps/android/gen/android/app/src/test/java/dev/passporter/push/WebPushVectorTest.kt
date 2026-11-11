package dev.passporter.push

import com.google.crypto.tink.apps.fixed_webpush.WebPushHybridDecrypt
import dev.passporter.notify.NoticeParse
import dev.passporter.notify.NoticeParser
import dev.passporter.notify.NoticeState
import java.security.GeneralSecurityException
import java.util.Base64
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

/**
 * The connector decrypts push messages with Tink's Web Push decrypter
 * (`WebPushHybridDecrypt`, built from its `DefaultKeyManager` keys). These
 * vectors prove it reads what the bridge encrypts (`crates/bridge/src/push/ece.rs`).
 * Robolectric for the platform's `org.json`.
 */
@RunWith(RobolectricTestRunner::class)
class WebPushVectorTest {
    private fun b64(text: String): ByteArray = Base64.getUrlDecoder().decode(text)

    /** The receiver of RFC 8291's example, whose keys the bridge's vector uses too. */
    private val receiver = WebPushHybridDecrypt.Builder()
        .withAuthSecret(b64(TestVector.auth))
        .withRecipientPublicKey(b64(TestVector.uaPublic))
        .withRecipientPrivateKey(b64(TestVector.uaPrivate))
        .build()

    @Test
    fun decryptsTheRfc8291Example() {
        // RFC 8291 Appendix A: the whole message.
        val body = b64(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        )
        assertEquals("When I grow up, I want to be a watermelon", String(receiver.decrypt(body, null), Charsets.UTF_8))
    }

    @Test
    fun decryptsWhatTheBridgeEncrypts() {
        val plaintext = String(receiver.decrypt(b64(TestVector.body), null), Charsets.UTF_8)
        assertEquals(TestVector.plaintext, plaintext)
        val parse = NoticeParser.parse(plaintext)
        check(parse is NoticeParse.Parsed) { "not parsed: $parse" }
        val notice = parse.message.notices.single()
        assertEquals("widget", notice.name)
        assertEquals(NoticeState.BLOCKED, notice.state)
        assertEquals("Run `pnpm test`? ✓", notice.line)
        assertEquals("screen-2-ab", notice.pendingId?.value)
    }

    @Test
    fun refusesATamperedMessage() {
        val body = b64(TestVector.body)
        body[body.size - 20] = (body[body.size - 20].toInt() xor 1).toByte()
        assertThrows(GeneralSecurityException::class.java) { receiver.decrypt(body, null) }
    }
}
