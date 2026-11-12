package dev.passporter.notify.actions

import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.notify.DEVICE
import dev.passporter.notify.PendingId
import dev.passporter.notify.agent

/** The fixed idempotency key of [job]s. */
const val KEY = "00000000-0000-4000-8000-000000000001"

/** A job of [action] on agent `w1:p1` (dialog `screen-2-ab` for an answer), taken at [createdAt]. */
fun job(action: NoticeAction, text: String? = null, createdAt: Long = 2000): OutboxJob {
    val pending = if (action.answersDialog) PendingId.parse("screen-2-ab") else null
    return OutboxJob(KEY, ActionRequest(action, agent("w1:p1"), pending), text, "widget", createdAt, DEVICE)
}

/** `GET /v1/agents` with `w1:p1` in [status], at seq 7 since [since]. */
fun agents(status: String = "blocked", since: Long = 1000, unread: Boolean = false): String =
    """[{"id":"w1:p1","title":"widget","repo":"r","status":"$status","seq":7,"statusSince":$since,
        "unread":$unread,"kind":"claude","extra":{"newer":true}}]"""

/** A Claude Code permission dialog `screen-2-ab`: allow, always allow (elevated), deny with feedback. */
const val PERMISSION =
    """{"id":"screen-2-ab","kind":"permission","prompt":"Run?","source":"hook","options":[
        {"id":"yes","label":"Yes","risk":"low"},
        {"id":"yes_always","label":"Yes, and don't ask again","risk":"elevated"},
        {"id":"no_feedback","label":"No, and tell Claude","risk":"low","acceptsFeedback":true}]}"""

/** One request a [FakeBridge] got. */
data class Sent(val method: Method, val path: String, val body: String?)

/** A [Bridge] that answers from [answer] and records every request. */
class FakeBridge(private val answer: (Sent) -> HttpResponse) : Bridge {
    val requests = mutableListOf<Sent>()

    override fun send(method: Method, path: String, json: String?): HttpResponse {
        val sent = Sent(method, path, json)
        requests += sent
        return answer(sent)
    }

    /** The bodies of `POST /v1/actions`. */
    val actions: List<String> get() = requests.filter { it.path == "/v1/actions" }.mapNotNull { it.body }

    companion object {
        /**
         * A bridge where `w1:p1` is [status] at [dialog], and actions get
         * [action].
         */
        fun of(
            status: String = "blocked",
            dialog: String = PERMISSION,
            since: Long = 1000,
            action: HttpResponse = HttpResponse(200, """{"status":"sent"}"""),
        ) = FakeBridge { sent ->
            when (sent.path) {
                "/v1/agents" -> HttpResponse(200, agents(status, since))
                "/v1/agents/w1:p1/pending" -> HttpResponse(200, dialog)
                "/v1/actions" -> action
                else -> HttpResponse(404, "")
            }
        }
    }
}

/** [FrozenBodies] in memory. */
class MemoryFrozen : FrozenBodies {
    val bodies = mutableMapOf<String, String>()

    override fun get(key: String) = bodies[key]

    override fun put(key: String, body: String) {
        bodies[key] = body
    }

    override fun remove(key: String) {
        bodies.remove(key)
    }

    override fun keys() = bodies.keys.toSet()

    override fun clear() = bodies.clear()
}
