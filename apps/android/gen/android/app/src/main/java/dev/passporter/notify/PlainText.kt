package dev.passporter.notify

/**
 * Untrusted text made fit for one notification line. The bridge already
 * does this thoroughly (`one_line`); this is the app's own floor, so a
 * notification never carries control or invisible characters, line breaks
 * or more than the bridge's bounds, whatever arrives.
 */
object PlainText {
    /**
     * [raw] with control and format characters (bidi controls, zero-width
     * characters, tags; not the joiner inside emoji) dropped, whitespace runs collapsed to one space,
     * trimmed, and cut to at most [max] characters (code points), an ending
     * ellipsis included. `null` if nothing is left.
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
        if (text.isEmpty()) return null
        val length = text.codePointCount(0, text.length)
        if (length <= max) return text
        val cut = text.offsetByCodePoints(0, (max - 1).coerceAtLeast(0))
        return text.substring(0, cut).trimEnd() + ELLIPSIS
    }

    private const val ELLIPSIS = "…"

    /** Joins emoji sequences; the bridge already drops misplaced ones. */
    private const val ZERO_WIDTH_JOINER = 0x200D

    private fun isHidden(codePoint: Int): Boolean =
        codePoint != ZERO_WIDTH_JOINER && when (Character.getType(codePoint).toByte()) {
            Character.CONTROL, Character.FORMAT, Character.SURROGATE, Character.PRIVATE_USE,
            -> true
            else -> false
        }
}
