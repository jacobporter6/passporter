package dev.passporter.device

import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class P256Test {
    private fun der(r: String, s: String): ByteArray {
        val ri = Hex.decode(r)
        val si = Hex.decode(s)
        val body = byteArrayOf(2, ri.size.toByte()) + ri + byteArrayOf(2, si.size.toByte()) + si
        return byteArrayOf(0x30, body.size.toByte()) + body
    }

    private val full = "11".repeat(32)

    @Test
    fun aHighBitIntegerLosesItsSignByte() {
        val r = "80" + "01".repeat(31)
        val p1363 = P256.derToP1363(der("00$r", full))
        assertEquals(r + full, Hex.encode(p1363))
    }

    @Test
    fun aShortIntegerIsLeftPadded() {
        // r with leading zero bytes in its 32-byte form: DER drops them.
        val p1363 = P256.derToP1363(der("7f" + "22".repeat(29), "01"))
        assertEquals("0000" + "7f" + "22".repeat(29) + "00".repeat(31) + "01", Hex.encode(p1363))
    }

    @Test
    fun bothHighBitAndShortTogether() {
        val s = "ff" + "33".repeat(31)
        val p1363 = P256.derToP1363(der("05", "00$s"))
        assertEquals("00".repeat(31) + "05" + s, Hex.encode(p1363))
    }

    @Test
    fun malformedDerIsRefused() {
        val bad = listOf(
            der("80" + "01".repeat(31), full), // negative r (sign bit set, no zero byte)
            der("0001", full), // non-minimal: leading zero not needed
            der("01" + "02".repeat(32), full), // 33 bytes without a sign byte: too large
            der(full, full) + byteArrayOf(0), // trailing byte
            der(full, full).copyOf(40), // truncated
            byteArrayOf(0x31, 0x00), // not a SEQUENCE
            byteArrayOf(0x30, 0x81.toByte(), 0x44), // long-form length
        )
        for (input in bad) {
            assertThrows(Hex.encode(input), IllegalArgumentException::class.java) { P256.derToP1363(input) }
        }
    }

    @Test
    fun keystoreShapedSignaturesVerifyAsP1363() {
        // Many signatures, so r and s with high bits and short forms all occur.
        val keys = SoftwareKeys()
        val key = keys.create()
        assertEquals(65, key.publicKey.size)
        assertEquals(4.toByte(), key.publicKey[0])
        repeat(200) { i ->
            val message = "message $i".toByteArray()
            val signature = keys.sign(key.alias, message)
            assertEquals(64, signature.size)
            assertTrue(keys.verifies(key.alias, message, signature))
        }
    }

    /** Shared with `tests/golden.rs`, which checks the same file against the Rust protocol and the bridge's verifier. */
    @Test
    fun theGoldenVectorMatchesTheProtocol() {
        val text = checkNotNull(javaClass.classLoader?.getResource("device-auth-golden.json")).readText()
        val golden = JSONObject(text)
        val device = checkNotNull(DeviceId.parse(golden.getString("deviceId")))
        val challenge = checkNotNull(Challenge.parse(golden.getString("challenge")))
        assertEquals(golden.getString("message"), Hex.encode(P256.signedMessage(device, challenge)))
        assertArrayEquals(
            Hex.decode(golden.getString("p1363")),
            P256.derToP1363(Hex.decode(golden.getString("der"))),
        )
    }
}
