package dev.passporter.push

import androidx.core.util.AtomicFile
import java.io.File
import java.io.IOException
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/** The subscription the bridge holds: [registration], for the device [deviceId]. */
data class Subscribed(
    /** What was posted. */
    val registration: Registration,
    /** The device it was posted as (`DeviceId.value`). */
    val deviceId: String,
)

/**
 * What [PushSync] remembers between runs: the distributor's latest
 * [registration], what the bridge holds ([subscribed]), so the next run
 * posts only what changed and knows which endpoint to drop, and the device
 * the current keys belong to ([owner]).
 */
data class PushRecord(
    /** The distributor's latest registration, or null before one (or after unregistering). */
    val registration: Registration? = null,
    /** What the bridge holds for this app, or null when nothing (that this app could still remove). */
    val subscribed: Subscribed? = null,
    /**
     * The device (`DeviceId.value`) the registration was asked for, set
     * before anything is posted: a bridge may hold the keys even when its
     * answer was lost, so a new pairing must replace them all the same.
     */
    val owner: String? = null,
    /**
     * Endpoints this app replaced or lost that the bridge may still hold
     * for [owner]: dropped there at the next chance, then forgotten.
     */
    val stale: List<String> = emptyList(),
)

/**
 * The [PushRecord], as one small JSON file in `noBackupFilesDir` (backups
 * and device transfers are off for the app anyway). It holds the public
 * keys and the auth secret, which the connector's own store keeps as well;
 * the private key is not here (see [Registration]).
 *
 * Safe from any thread: reads and writes take one lock, and the record is
 * kept in memory once read, so a reader (a push message, on the main
 * thread from cold) costs no disk read after the first.
 */
class PushStore(file: File) {
    private val file = AtomicFile(file)
    private val lock = Any()
    private var cached: PushRecord? = null

    /** The record; empty when there is none, or the file does not read back as one. */
    fun load(): PushRecord = synchronized(lock) {
        cached ?: read().also { cached = it }
    }

    /** Replaces the record. Atomic: a crash leaves the old file or the new one. */
    fun save(record: PushRecord): Unit = synchronized(lock) {
        // Forgotten first: if the write fails, the next load reads the disk.
        cached = null
        write(record)
        cached = record
    }

    private fun read(): PushRecord {
        val text = try {
            file.readFully().toString(Charsets.UTF_8)
        } catch (e: IOException) {
            return PushRecord()
        }
        return try {
            val obj = JSONObject(text)
            val registration = obj.optJSONObject(REGISTRATION)?.let(::registration)
            val subscribed = obj.optJSONObject(SUBSCRIBED)?.let { sub ->
                val deviceId = sub.optString(DEVICE_ID).takeIf { it.isNotEmpty() }
                val posted = registration(sub)
                if (deviceId != null && posted != null) Subscribed(posted, deviceId) else null
            }
            val stale = obj.optJSONArray(STALE)?.let { array ->
                (0 until array.length()).mapNotNull { array.optString(it).takeIf(String::isNotEmpty) }
            }.orEmpty()
            PushRecord(registration, subscribed, obj.optString(OWNER).takeIf { it.isNotEmpty() }, stale)
        } catch (e: JSONException) {
            PushRecord()
        }
    }

    private fun write(record: PushRecord) {
        val obj = JSONObject()
        record.registration?.let { obj.put(REGISTRATION, json(it)) }
        record.subscribed?.let { obj.put(SUBSCRIBED, json(it.registration).put(DEVICE_ID, it.deviceId)) }
        record.owner?.let { obj.put(OWNER, it) }
        if (record.stale.isNotEmpty()) obj.put(STALE, JSONArray(record.stale))
        file.baseFile.parentFile?.mkdirs()
        val out = file.startWrite()
        try {
            out.write(obj.toString().toByteArray(Charsets.UTF_8))
            file.finishWrite(out)
        } catch (e: IOException) {
            file.failWrite(out)
            throw e
        }
    }

    private fun json(registration: Registration) = JSONObject()
        .put(ENDPOINT, registration.endpoint)
        .put(P256DH, registration.p256dh)
        .put(AUTH, registration.auth)

    private fun registration(obj: JSONObject): Registration? =
        Registration.of(obj.optString(ENDPOINT), obj.optString(P256DH), obj.optString(AUTH))

    private companion object {
        const val REGISTRATION = "registration"
        const val SUBSCRIBED = "subscribed"
        const val DEVICE_ID = "deviceId"
        const val OWNER = "owner"
        const val STALE = "stale"
        const val ENDPOINT = "endpoint"
        const val P256DH = "p256dh"
        const val AUTH = "auth"
    }
}
