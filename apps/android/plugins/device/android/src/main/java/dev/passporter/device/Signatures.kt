package dev.passporter.device

import java.math.BigInteger
import java.security.interfaces.ECPublicKey

/** Lowercase hex, as device-auth binary values travel. */
internal object Hex {
    fun encode(bytes: ByteArray): String = bytes.joinToString("") { "%02x".format(it.toInt() and 0xff) }

    fun decode(text: String): ByteArray {
        require(text.length % 2 == 0) { "odd hex length" }
        return ByteArray(text.length / 2) { i -> text.substring(2 * i, 2 * i + 2).toInt(16).toByte() }
    }
}

/** P-256 ECDSA encodings: what Android's Keystore gives, and what the bridge wants. */
internal object P256 {
    /** Bytes in one field element (a coordinate, `r` or `s`). */
    const val FIELD_BYTES = 32

    /** The protocol's prefix (`protocol::SIGNED_PREFIX`). */
    private const val SIGNED_PREFIX = "passporter-session-v1:"

    /**
     * The exact bytes [device] signs for [challenge], as
     * `protocol::signed_message`: `passporter-session-v1:{device}:{challenge}`.
     */
    fun signedMessage(device: DeviceId, challenge: Challenge): ByteArray =
        "$SIGNED_PREFIX${device.value}:${challenge.value}".toByteArray(Charsets.US_ASCII)

    /** [key] as SEC1 uncompressed (`04 || x || y`, 65 bytes), as WebCrypto exports it. */
    fun sec1(key: ECPublicKey): ByteArray =
        byteArrayOf(4) + fixed(key.w.affineX) + fixed(key.w.affineY)

    /**
     * A DER `ECDSA-Sig-Value` (`SEQUENCE { r INTEGER, s INTEGER }`, what
     * `SHA256withECDSA` returns) as IEEE P1363 `r || s`, 32 bytes each.
     * Strict: anything but a minimal DER encoding of two positive integers
     * that fit in 32 bytes is refused.
     */
    fun derToP1363(der: ByteArray): ByteArray {
        val reader = DerReader(der)
        reader.expect(0x30, "SEQUENCE")
        val length = reader.length()
        require(length == der.size - reader.position) { "SEQUENCE length" }
        val r = reader.positiveInteger()
        val s = reader.positiveInteger()
        require(reader.position == der.size) { "trailing bytes" }
        return r + s
    }

    /** A non-negative integer as exactly [FIELD_BYTES] big-endian bytes. */
    private fun fixed(value: BigInteger): ByteArray {
        val bytes = value.toByteArray()
        // `toByteArray` adds a sign byte when the top bit is set.
        val magnitude = if (bytes.size > 1 && bytes[0] == 0.toByte()) bytes.copyOfRange(1, bytes.size) else bytes
        require(value.signum() >= 0 && magnitude.size <= FIELD_BYTES) { "not a P-256 field element" }
        return ByteArray(FIELD_BYTES - magnitude.size) + magnitude
    }

    private class DerReader(private val bytes: ByteArray) {
        var position = 0
            private set

        private fun next(): Int {
            require(position < bytes.size) { "truncated" }
            return bytes[position++].toInt() and 0xff
        }

        fun expect(tag: Int, what: String) = require(next() == tag) { "expected $what" }

        /** A short-form length: a P-256 signature never needs the long form. */
        fun length(): Int = next().also { require(it < 0x80) { "long-form length" } }

        fun positiveInteger(): ByteArray {
            expect(0x02, "INTEGER")
            val length = length()
            require(length in 1..FIELD_BYTES + 1 && position + length <= bytes.size) { "INTEGER length" }
            val value = bytes.copyOfRange(position, position + length)
            position += length
            val first = value[0].toInt() and 0xff
            require(first and 0x80 == 0) { "negative INTEGER" }
            if (first == 0 && length > 1) {
                // A leading zero is only there to clear the sign bit.
                require(value[1].toInt() and 0x80 != 0) { "non-minimal INTEGER" }
            }
            val magnitude = if (first == 0 && length > 1) value.copyOfRange(1, length) else value
            require(magnitude.size <= FIELD_BYTES) { "INTEGER too large" }
            return ByteArray(FIELD_BYTES - magnitude.size) + magnitude
        }
    }
}
