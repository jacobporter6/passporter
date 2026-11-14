package dev.passporter.push

import androidx.core.util.AtomicFile
import java.io.File
import java.io.IOException
import org.json.JSONException
import org.json.JSONObject

/** Which transport brings agent news ([NativeRecord.choice]). */
enum class Transport {
    /** The bridge's own (`POST /v1/push/native`) when it has one, else an external distributor. */
    BUILT_IN,

    /** Always the external UnifiedPush distributor (ntfy), as before the built-in one. */
    EXTERNAL,
}

/**
 * What the built-in receiver remembers ([NativeReceiver]): the transport
 * you chose, and for the pairing that owns it ([owner]) its registration,
 * the bridge's signing key and how far the stream was read.
 */
data class NativeRecord(
    /** Your choice; kept across pairings. */
    val choice: Transport = Transport.BUILT_IN,
    /** The device (`DeviceId.value`) the keys and registration were made for. */
    val owner: String? = null,
    /** The registration id, a UUID this app picked; null until registered. */
    val registrationId: String? = null,
    /** The key the bridge signs this registration's envelopes with. */
    val signingKey: String? = null,
    /** The id of the last message read from the stream (ntfy's), or null for all. */
    val cursor: String? = null,
) {
    /** Whether the bridge holds a registration for [owner]. */
    val registered: Boolean get() = owner != null && registrationId != null && signingKey != null

    /** The same choice, nothing of any pairing. */
    fun forgotten(): NativeRecord = NativeRecord(choice = choice)
}

/**
 * The [NativeRecord], as one small JSON file in `noBackupFilesDir` (backups
 * and device transfers leave the app's data out). It holds no secret: the
 * receiver's private key and auth secret are the connector's key manager's.
 *
 * Safe from any thread; the record is cached once read, so a reader on the
 * main thread costs no disk read after the first.
 */
class NativeStore(file: File) {
    private val file = AtomicFile(file)
    private val lock = Any()
    private var cached: NativeRecord? = null

    /** The record; an empty one when there is none, or it does not read back. */
    fun load(): NativeRecord = synchronized(lock) {
        cached ?: read().also { cached = it }
    }

    /** Replaces the record. Atomic: a crash leaves the old file or the new one. */
    fun save(record: NativeRecord): Unit = synchronized(lock) {
        cached = null
        write(record)
        cached = record
    }

    private fun read(): NativeRecord {
        val text = try {
            file.readFully().toString(Charsets.UTF_8)
        } catch (e: IOException) {
            return NativeRecord()
        }
        return try {
            val obj = JSONObject(text)
            val choice = Transport.entries.firstOrNull { it.name == obj.optString(CHOICE) } ?: Transport.BUILT_IN
            NativeRecord(
                choice = choice,
                owner = obj.text(OWNER),
                registrationId = obj.text(REGISTRATION),
                signingKey = obj.text(SIGNING_KEY)?.takeIf(NativeEnvelope::isSigningKey),
                cursor = obj.text(CURSOR)?.takeIf(NativeReceiver::isCursor),
            )
        } catch (e: JSONException) {
            NativeRecord()
        }
    }

    private fun write(record: NativeRecord) {
        val obj = JSONObject().put(CHOICE, record.choice.name)
        record.owner?.let { obj.put(OWNER, it) }
        record.registrationId?.let { obj.put(REGISTRATION, it) }
        record.signingKey?.let { obj.put(SIGNING_KEY, it) }
        record.cursor?.let { obj.put(CURSOR, it) }
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

    private fun JSONObject.text(key: String): String? = optString(key).takeIf { it.isNotEmpty() }

    private companion object {
        const val CHOICE = "choice"
        const val OWNER = "owner"
        const val REGISTRATION = "registrationId"
        const val SIGNING_KEY = "signingKey"
        const val CURSOR = "cursor"
    }
}
