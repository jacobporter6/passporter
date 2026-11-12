package dev.passporter.car

import android.content.Context
import dev.passporter.device.DeviceAuth
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.device.Scope
import dev.passporter.notify.PlainText
import dev.passporter.notify.actions.Bridge
import org.json.JSONException
import org.json.JSONObject

/**
 * [Bridge] with a **car**-scope session: the bridge judges everything it
 * sends as `origin: "car"` and lets it reach only the car-safe routes
 * (design §6.6, §8), whatever the app put in the body.
 */
internal class CarBridge(
    private val auth: (Scope, Method, String, String?) -> HttpResponse,
) : Bridge {
    /** Over the app's device identity ([DeviceAuth.send]). */
    constructor(context: Context) : this(DeviceAuth.get(context)::send)

    override fun send(method: Method, path: String, json: String?): HttpResponse = auth(Scope.CAR, method, path, json)
}

/** Why the app itself answers the car, when the bridge's sentence is not there to read. */
enum class CarTrouble {
    /** Nothing was heard but filler or invisible characters. */
    NOT_HEARD,

    /** Longer than the bridge's utterance limit. */
    TOO_LONG,

    /** The app is not (or no longer) paired. */
    NOT_PAIRED,

    /**
     * The app was unpaired or paired anew since the car heard the notice
     * ([CarJob.device]): the reply belongs to the old pairing, and is never
     * sent to the new one.
     */
    PAIRING_CHANGED,

    /** The bridge stayed unreachable for every retry. */
    UNREACHABLE,

    /** The bridge stayed unreachable until the reply was too old to send ([CarOutbox.MAX_AGE_MS]). */
    TOO_OLD,

    /** The bridge answered, but not with a reply this app can read: it may have been sent. */
    UNCERTAIN,

    /** Anything else: refused without a sentence, a broken key store. */
    FAILED,
}

/** What one attempt at a [CarJob] came to. */
sealed interface CarOutcome {
    /** The bridge answered for the key: read [speech] aloud. Never retried. */
    data class Spoken(
        /** The bridge's sentence, cleaned to one line ([CarReplySender.MAX_SPEECH_CHARS]). */
        val speech: String,
        /** Whether it was sent to the agent (`result.status` is `sent`). */
        val sent: Boolean,
    ) : CarOutcome

    /** Final without a sentence from the bridge. */
    data class Trouble(
        /** Why. */
        val trouble: CarTrouble,
    ) : CarOutcome

    /**
     * Nothing decided: send the same body again later, after WorkManager's
     * backoff (a `Retry-After` is not followed, [CarReplyWorker]).
     */
    data object Retry : CarOutcome
}

/**
 * Sends one [CarJob] as `POST /v1/agents/:id/car-reply` over a car-scope
 * [Bridge], and reads the answer (`protocol::CarReply`: a sentence to read
 * aloud and the action's result). The outbox contract of
 * `POST /v1/actions` holds: 2xx and 409 are final for the key, as are the
 * refusals (400, 403, 404, 422); 429 and 5xx decided nothing (503 is the
 * bridge saying herdr or its audit log was unavailable) and are retried
 * with the same body. Every attempt first checks that the app is still
 * paired as the job's device ([paired]); else nothing is sent
 * ([CarTrouble.PAIRING_CHANGED]).
 */
class CarReplySender(
    private val bridge: Bridge,
    /** The device the app is paired as now (`dev.passporter.notify.actions.pairedDevice`). */
    private val paired: () -> DeviceId?,
) {
    /** One attempt at [job]. Never throws. */
    fun send(job: CarJob): CarOutcome =
        if (job.device != paired()) {
            CarOutcome.Trouble(CarTrouble.PAIRING_CHANGED)
        } else {
            try {
                answer(bridge.send(Method.POST, job.path, job.body()))
            } catch (e: DeviceAuthException) {
                failure(e)
            }
        }

    private fun answer(response: HttpResponse): CarOutcome {
        val status = response.status
        return when {
            status == 429 || status in 500..599 -> CarOutcome.Retry
            status == 401 -> CarOutcome.Trouble(CarTrouble.NOT_PAIRED)
            response.isSuccess || status in FINAL -> spoken(response.body)
                ?: CarOutcome.Trouble(if (response.isSuccess || status == 409) CarTrouble.UNCERTAIN else CarTrouble.FAILED)
            else -> CarOutcome.Trouble(CarTrouble.FAILED)
        }
    }

    /** The bridge's [CarOutcome.Spoken] in [body], or `null` if it holds none. */
    private fun spoken(body: String): CarOutcome.Spoken? =
        try {
            val reply = JSONObject(body)
            val speech = (reply.opt("speech") as? String)?.let { PlainText.oneLine(it, MAX_SPEECH_CHARS) }
            val sent = reply.optJSONObject("result")?.opt("status") == "sent"
            speech?.let { CarOutcome.Spoken(it, sent) }
        } catch (_: JSONException) {
            null
        }

    /** A failure to get an answer at all, from [DeviceAuth]: as the shade's outbox treats it. */
    private fun failure(e: DeviceAuthException): CarOutcome =
        when (e) {
            is DeviceAuthException.Unreachable, is DeviceAuthException.ServerError, is DeviceAuthException.Refused,
            is DeviceAuthException.RateLimited,
            -> CarOutcome.Retry
            is DeviceAuthException.NotPaired, is DeviceAuthException.Forgotten, is DeviceAuthException.NoBridge ->
                CarOutcome.Trouble(CarTrouble.NOT_PAIRED)
            is DeviceAuthException.TooManyDevices, is DeviceAuthException.BadResponse, is DeviceAuthException.Keystore,
            is DeviceAuthException.Storage, is DeviceAuthException.InvalidInput,
            -> CarOutcome.Trouble(CarTrouble.FAILED)
        }

    companion object {
        /**
         * Most characters of a sentence read aloud: the bridge's are one
         * short sentence (an allowed command is cut at 80).
         */
        const val MAX_SPEECH_CHARS = 200

        /** Refusals final for the key, each with a sentence: invalid, car policy, unknown agent, conflict. */
        private val FINAL = setOf(400, 403, 404, 409, 422)
    }
}
