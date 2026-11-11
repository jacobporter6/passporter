package dev.passporter.push

import java.io.File
import java.io.FileOutputStream
import java.io.IOException
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
 * [registration], and what the bridge holds ([subscribed]), so the next
 * run posts only what changed and knows which endpoint to drop.
 */
data class PushRecord(
    /** The distributor's latest registration, or null before one (or after unregistering). */
    val registration: Registration? = null,
    /** What the bridge holds for this app, or null when nothing (that this app could still remove). */
    val subscribed: Subscribed? = null,
)

/**
 * The [PushRecord], as one small JSON file in `noBackupFilesDir` (backups
 * and device transfers are off for the app anyway). It holds the public
 * keys and the auth secret, which the connector's own store keeps as well;
 * the private key is not here (see [Registration]).
 */
class PushStore(private val file: File) {
    /** The record; empty when there is none, or the file does not read back as one. */
    fun load(): PushRecord {
        val text = try {
            file.readText()
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
            PushRecord(registration, subscribed)
        } catch (e: JSONException) {
            PushRecord()
        }
    }

    /** Replaces the record. Atomic: a crash leaves the old file or the new one. */
    fun save(record: PushRecord) {
        val obj = JSONObject()
        record.registration?.let { obj.put(REGISTRATION, json(it)) }
        record.subscribed?.let { obj.put(SUBSCRIBED, json(it.registration).put(DEVICE_ID, it.deviceId)) }
        file.parentFile?.mkdirs()
        val partial = File(file.path + ".partial")
        FileOutputStream(partial).use { out ->
            out.write(obj.toString().toByteArray(Charsets.UTF_8))
            out.fd.sync()
        }
        if (!partial.renameTo(file)) throw IOException("could not replace $file")
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
        const val ENDPOINT = "endpoint"
        const val P256DH = "p256dh"
        const val AUTH = "auth"
    }
}
