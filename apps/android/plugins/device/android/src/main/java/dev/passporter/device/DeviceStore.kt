package dev.passporter.device

import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import org.json.JSONException
import org.json.JSONObject

/**
 * What pairing left behind: the bridge's id for this device, the Keystore
 * alias of its key, and the bridge it paired with.
 */
internal class DeviceRecord(val deviceId: DeviceId, val keyAlias: String, val origin: BridgeOrigin)

/**
 * The [DeviceRecord], as one small JSON file. The app keeps it in
 * `noBackupFilesDir`, and backups and device transfers are off for the app
 * anyway, so a pairing never leaves the phone (the key cannot).
 */
internal class DeviceStore(private val file: File) {
    /** The record, or null before pairing. A file that does not read back as one is deleted. */
    fun load(): DeviceRecord? {
        val text = try {
            file.readText()
        } catch (e: IOException) {
            return null
        }
        val record = try {
            val obj = JSONObject(text)
            val id = DeviceId.parse(obj.getString("deviceId"))
            val alias = obj.getString("keyAlias").takeIf { it.isNotEmpty() }
            val origin = BridgeOrigin.parse(obj.getString("origin"))
            if (id != null && alias != null && origin != null) DeviceRecord(id, alias, origin) else null
        } catch (e: JSONException) {
            null
        }
        if (record == null) clear()
        return record
    }

    /** Replaces the record. Atomic: a crash leaves the old file or the new one. */
    fun save(record: DeviceRecord) {
        file.parentFile?.mkdirs()
        val partial = File(file.path + ".partial")
        val text = JSONObject()
            .put("deviceId", record.deviceId.value)
            .put("keyAlias", record.keyAlias)
            .put("origin", record.origin.toString())
            .toString()
        FileOutputStream(partial).use { out ->
            out.write(text.toByteArray(Charsets.UTF_8))
            // On disk before the rename, so a power cut cannot leave an
            // empty record (which would be dropped, orphaning the pairing).
            out.fd.sync()
        }
        if (!partial.renameTo(file)) throw IOException("could not replace $file")
    }

    /** Deletes the record. */
    fun clear() {
        file.delete()
    }
}
