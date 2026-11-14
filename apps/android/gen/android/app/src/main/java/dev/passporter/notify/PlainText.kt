package dev.passporter.notify

/**
 * Untrusted text made fit for one notification line, by the bridge's rule
 * (`one_line`, `protocol::text::is_hidden_char`). The bridge already
 * cleans what it sends; this is the app's own floor, so a notification
 * never carries control or invisible characters, line breaks or more than
 * the bridge's bounds. (The bridge also drops misplaced joiners and
 * selectors; that only tidies text, so the app does not repeat it.)
 */
object PlainText {
    /**
     * [raw] with control and hidden characters ([isHidden]) dropped,
     * whitespace runs collapsed to one space, trimmed, and cut to at most
     * [max] characters (code points), an ending ellipsis included. `null`
     * if nothing visible is left.
     */
    fun oneLine(raw: String, max: Int): String? {
        val words = StringBuilder()
        var pendingSpace = false
        var index = 0
        while (index < raw.length) {
            val codePoint = raw.codePointAt(index)
            index += Character.charCount(codePoint)
            when {
                Character.isWhitespace(codePoint) || Character.isSpaceChar(codePoint) ->
                    pendingSpace = words.isNotEmpty()
                isHidden(codePoint) -> Unit
                else -> {
                    if (pendingSpace) words.append(' ')
                    pendingSpace = false
                    words.appendCodePoint(codePoint)
                }
            }
        }
        val text = words.toString()
        if (isBlank(text)) return null
        val length = text.codePointCount(0, text.length)
        if (length <= max) return text
        val cut = text.offsetByCodePoints(0, (max - 1).coerceAtLeast(0))
        return text.substring(0, cut).trimEnd() + ELLIPSIS
    }

    private const val ELLIPSIS = "…"

    /** The zero-width joiners: they shape emoji and Indic or Persian text. */
    private fun isJoiner(codePoint: Int): Boolean = codePoint == 0x200C || codePoint == 0x200D

    /** Emoji variation selectors: allowed, but invisible on their own. */
    private fun isVariationSelector(codePoint: Int): Boolean = codePoint in 0xFE00..0xFE0F

    /** Whether nothing visible is left: only joiners and selectors. */
    private fun isBlank(text: String): Boolean =
        text.codePoints().allMatch { it == ' '.code || isJoiner(it) || isVariationSelector(it) }

    /**
     * The bridge's `protocol::text::is_hidden_char` plus control characters:
     * format characters other than the joiners, the invisible fillers and
     * reserved characters it lists; also lone surrogates and private use.
     */
    internal fun isHidden(codePoint: Int): Boolean {
        val category = Character.getType(codePoint).toByte()
        return when {
            category == Character.FORMAT -> !isJoiner(codePoint)
            category == Character.CONTROL || category == Character.SURROGATE ||
                category == Character.PRIVATE_USE -> true
            else -> INVISIBLE.any { codePoint in it }
        }
    }

    /** `protocol::text::is_invisible_or_reserved`. */
    private val INVISIBLE = listOf(
        0x034F..0x034F, 0x115F..0x1160, 0x17B4..0x17B5, 0x180B..0x180F, 0x2065..0x2065,
        0x3164..0x3164, 0xFFA0..0xFFA0, 0xFFF0..0xFFF8, 0xE0000..0xE0FFF,
    )
}
