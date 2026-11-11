package dev.passporter.car

import android.net.Uri
import androidx.work.Data
import org.json.JSONObject
import java.util.UUID

/**
 * One spoken reply from the car, kept until the bridge has it: a durable
 * job whose body is fixed when the utterance arrives (key, utterance,
 * pending id), so every retry sends the same request and the bridge
 * replays its first answer.
 */
data class CarJob(
    /** The reply's `idempotencyKey`: a lowercase UUID, fixed for life. */
    val key: String,
    /** The agent and what the car read aloud ([CarRequest.action] is [CarAction.REPLY]). */
    val request: CarRequest,
    /** The utterance as heard, made fit for the bridge's `Utterance` ([CarReplyReceiver]). */
    val utterance: String,
    /** The agent's name when the car heard the notice. Display only. */
    val name: String,
) {
    init {
        require(request.action == CarAction.REPLY) { "a car job is a reply" }
    }

    /**
     * The body of `POST /v1/agents/:id/car-reply` (`protocol::CarReplyRequest`):
     * the utterance as heard and the pending id the car read aloud, if
     * any. The app adds nothing: what it means is the bridge's call.
     */
    fun body(): String =
        JSONObject()
            .put("idempotencyKey", key)
            .put("utterance", utterance)
            .apply { request.pending?.let { put("pendingId", it.value) } }
            .toString()

    /** The path it is posted to. */
    val path: String get() = "/v1/agents/${request.agent.value}/car-reply"

    companion object {
        /** A new job for [request], with a fresh key. */
        fun create(request: CarRequest, utterance: String, name: String): CarJob =
            CarJob(UUID.randomUUID().toString(), request, utterance, name)
    }
}

/** A [CarJob] as WorkManager input [Data]. */
internal object CarJobData {
    private const val KEY = "key"
    private const val REQUEST = "request"
    private const val UTTERANCE = "utterance"
    private const val NAME = "name"

    /** The input of [job]. */
    fun write(job: CarJob): Data =
        Data.Builder()
            .putString(KEY, job.key)
            // The request as its intent data spells it.
            .putString(REQUEST, job.request.toUri().toString())
            .putString(UTTERANCE, job.utterance)
            .putString(NAME, job.name)
            .build()

    /** The job in [data], or `null` if it is not one. */
    fun read(data: Data): CarJob? {
        val request = data.getString(REQUEST)?.let { CarRequest.parse(Uri.parse(it)) } ?: return null
        if (request.action != CarAction.REPLY) return null
        return CarJob(
            key = data.getString(KEY) ?: return null,
            request = request,
            utterance = data.getString(UTTERANCE) ?: return null,
            name = data.getString(NAME) ?: return null,
        )
    }
}
