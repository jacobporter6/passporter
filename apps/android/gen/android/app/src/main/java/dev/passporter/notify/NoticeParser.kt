package dev.passporter.notify

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/** The outcome of [NoticeParser.parse]. */
sealed interface NoticeParse {
    /** A well-formed message. */
    data class Parsed(
        /** The message. */
        val message: NoticeMessage,
    ) : NoticeParse

    /** Input the bridge would never send; nothing should be shown from it. */
    data class Invalid(
        /** What was wrong. */
        val error: NoticeError,
    ) : NoticeParse
}

/**
 * Why a payload is not a notice message. Names the field only, never its
 * value: the payload is agent output and stays out of logs.
 */
sealed interface NoticeError {
    /** Where in the payload, e.g. `notices[1].agentId`; empty for the whole. */
    val field: String

    /** The payload is not a JSON object. */
    data object NotAnObject : NoticeError {
        override val field = ""
    }

    /** A required field is absent or null. */
    data class Missing(override val field: String) : NoticeError

    /** A field has the wrong JSON type. */
    data class WrongType(override val field: String) : NoticeError

    /** An id field is outside the bridge's identifier charset. */
    data class InvalidId(override val field: String) : NoticeError
}

/**
 * Parses the decrypted plaintext of a push message into a [NoticeMessage].
 *
 * The policy is the bridge's for outbound types (`protocol::push`):
 * tolerant of what a newer bridge may add (unknown fields are ignored, an
 * unknown state is [NoticeState.UNKNOWN]), strict about what is there (a
 * wrong type or an invalid id rejects the whole message, as serde would).
 * Texts are cleaned and bounded again ([PlainText]) so a notification never
 * shows more than the bridge meant to send.
 */
object NoticeParser {
    /** The message in [payload], or why there is none. Never throws. */
    fun parse(payload: String): NoticeParse {
        val root = try {
            JSONObject(payload)
        } catch (_: JSONException) {
            return NoticeParse.Invalid(NoticeError.NotAnObject)
        }
        return try {
            NoticeParse.Parsed(message(root))
        } catch (failure: Failure) {
            NoticeParse.Invalid(failure.error)
        }
    }

    private class Failure(val error: NoticeError) : Exception()

    private fun message(root: JSONObject): NoticeMessage {
        val ping = root.required<String>("ping", "ping")
        val notices = root.optional<JSONArray>("notices", "notices")
        val list = (0 until (notices?.length() ?: 0)).map { index ->
            val path = "notices[$index]"
            val raw = notices?.opt(index)
            notice(raw as? JSONObject ?: throw Failure(NoticeError.WrongType(path)), path)
        }
        return NoticeMessage(ping, list)
    }

    private fun notice(json: JSONObject, path: String): Notice {
        fun field(name: String) = "$path.$name"
        val agentId = json.required<String>("agentId", field("agentId")).let {
            AgentId.parse(it) ?: throw Failure(NoticeError.InvalidId(field("agentId")))
        }
        val pendingId = json.optional<String>("pendingId", field("pendingId"))?.let {
            PendingId.parse(it) ?: throw Failure(NoticeError.InvalidId(field("pendingId")))
        }
        val name = json.required<String>("name", field("name"))
        val line = json.optional<String>("line", field("line"))
        val decide = json.optional<String>("decide", field("decide"))
        return Notice(
            agentId = agentId,
            name = PlainText.oneLine(name, Notice.MAX_NAME_CHARS) ?: FALLBACK_NAME,
            state = NoticeState.fromWire(json.required<String>("state", field("state"))),
            line = line?.let { PlainText.oneLine(it, Notice.MAX_LINE_CHARS) },
            decide = decide?.let { PlainText.oneLine(it, Notice.MAX_LINE_CHARS) },
            pendingId = pendingId,
            announce = json.optional<Boolean>("announce", field("announce")) ?: false,
        )
    }

    /** What the bridge calls an agent whose title has nothing visible. */
    private const val FALLBACK_NAME = "Agent"

    private inline fun <reified T : Any> JSONObject.required(key: String, path: String): T =
        optional<T>(key, path) ?: throw Failure(NoticeError.Missing(path))

    /** The value at [key]; absent and `null` are both `null` (as serde's `Option`). */
    private inline fun <reified T : Any> JSONObject.optional(key: String, path: String): T? =
        when (val value = opt(key)) {
            null, JSONObject.NULL -> null
            is T -> value
            else -> throw Failure(NoticeError.WrongType(path))
        }
}
