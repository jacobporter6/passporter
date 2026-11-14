package dev.passporter.device

/** Lowercase hex back to bytes, for test vectors. */
internal fun Hex.decode(text: String): ByteArray {
    require(text.length % 2 == 0) { "odd hex length" }
    return ByteArray(text.length / 2) { i -> text.substring(2 * i, 2 * i + 2).toInt(16).toByte() }
}
