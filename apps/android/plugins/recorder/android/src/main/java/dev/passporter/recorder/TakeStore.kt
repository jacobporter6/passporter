package dev.passporter.recorder

import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.RandomAccessFile
import org.json.JSONException
import org.json.JSONObject

/**
 * Takes on disk, in [dir] (`noBackupFilesDir/recordings`: app-private, never
 * backed up). Each take is two files: `<id>.aac`, the audio, which
 * `MediaRecorder` appends to frame by frame while it records, and
 * `<id>.json`, its journal, written (atomically) before recording starts
 * and again when it ends. A journal still saying `recording` when the app
 * starts belongs to a process that died mid-take: [open] finishes it from
 * what its audio file holds.
 *
 * Not thread-safe: [Recorder] serializes every call.
 */
internal class TakeStore(private val dir: File) {
    /** The audio file of take [id]. */
    fun audio(id: RecordingId) = File(dir, "$id$AUDIO")

    private fun journal(id: RecordingId) = File(dir, "$id$JOURNAL")

    /**
     * Readies take [id]: writes its journal and answers the (empty) file to
     * record into.
     *
     * @throws RecorderException.Storage when the journal cannot be written.
     */
    fun begin(id: RecordingId, tag: Tag, startedAtMs: Long): File {
        storage {
            if (!dir.isDirectory && !dir.mkdirs()) throw IOException("cannot create $dir")
            write(id, JSONObject().put("id", id.value).put("tag", tag.value).put("startedAt", startedAtMs).put("state", RECORDING))
        }
        return audio(id).also { it.delete() }
    }

    /**
     * Ends take [id] for [reason]: cuts any torn last frame off its audio
     * and records its length. A take without a whole frame is deleted.
     *
     * @return the take, or null when nothing was recorded (or it is unknown).
     * @throws RecorderException.Storage when the files cannot be updated.
     */
    fun finish(id: RecordingId, reason: EndReason, endedAtMs: Long): Take? = storage {
        val entry = readJournal(id) ?: return@storage null
        val tag = entry.optString("tag").let(Tag::parse) ?: run { delete(id); return@storage null }
        val file = audio(id)
        val scan = if (file.isFile) Adts.scan(file) else AdtsScan(0, 0, 0)
        if (scan.frames == 0L) {
            delete(id)
            return@storage null
        }
        // The audio reaches the disk before the journal vouches for its size.
        RandomAccessFile(file, "rw").use {
            if (it.length() > scan.validBytes) it.setLength(scan.validBytes)
            it.fd.sync()
        }
        val take = Take(id, tag, scan.durationMs.coerceAtLeast(1), scan.validBytes, reason, endedAtMs)
        write(id, entry.put("state", FINISHED).put("durationMs", take.durationMs).put("bytes", take.bytes)
            .put("endedBy", reason.wire).put("endedAt", endedAtMs))
        take
    }

    /**
     * Reads the takes on disk at app start, oldest first: finishes those a
     * dead process left recording ([EndReason.INTERRUPTED], ended when
     * their audio was last written), deletes takes that ended more than
     * [retentionMs] before [nowMs], and anything else that is not a whole
     * take (an unreadable journal, audio without one, a half-written journal).
     */
    fun open(nowMs: Long, retentionMs: Long): List<Take> {
        val files = dir.listFiles() ?: return emptyList()
        val takes = mutableListOf<Take>()
        val known = mutableSetOf<String>()
        for (file in files) {
            if (!file.name.endsWith(JOURNAL)) continue
            val id = RecordingId.parse(file.name.removeSuffix(JOURNAL)) ?: continue
            known += id.value
            val take = try {
                load(id, nowMs)
            } catch (e: RecorderException.Storage) {
                // Storage failing now (a full disk) says nothing about the
                // take: leave its files for the next start.
                continue
            }
            when {
                take == null -> delete(id)
                take.endedAtMs < nowMs - retentionMs -> delete(id)
                else -> takes += take
            }
        }
        for (file in files) {
            val stem = file.name.substringBefore('.')
            if (stem !in known || (!file.name.endsWith(JOURNAL) && !file.name.endsWith(AUDIO))) file.delete()
        }
        return takes.sortedBy { it.endedAtMs }
    }

    /** One take from its journal, finishing it if a dead process left it recording. */
    private fun load(id: RecordingId, nowMs: Long): Take? {
        val entry = storage { readJournal(id) } ?: return null
        return when (entry.optString("state")) {
            RECORDING -> {
                val written = audio(id).lastModified().takeIf { it > 0 } ?: nowMs
                finish(id, EndReason.INTERRUPTED, written)
            }
            FINISHED -> {
                val tag = Tag.parse(entry.optString("tag")) ?: return null
                val reason = EndReason.of(entry.optString("endedBy")) ?: return null
                val bytes = entry.optLong("bytes", -1)
                val durationMs = entry.optLong("durationMs", 0)
                if (bytes <= 0 || durationMs <= 0) return null
                val endedAt = entry.optLong("endedAt")
                // An audio file that does not match (its last writes lost
                // to a power cut, say) is finished again from its frames.
                if (audio(id).length() != bytes) return finish(id, reason, endedAt)
                Take(id, tag, durationMs, bytes, reason, endedAt)
            }
            else -> null
        }
    }

    /**
     * Up to [length] bytes of take [id]'s audio from [offset] (fewer at the
     * end; none past it).
     *
     * @throws RecorderException.NotFound when the take has no audio.
     */
    fun read(id: RecordingId, offset: Long, length: Int): ByteArray {
        val file = audio(id)
        if (!file.isFile) throw RecorderException.NotFound()
        return storage {
            RandomAccessFile(file, "r").use { input ->
                val count = (input.length() - offset).coerceIn(0, length.toLong()).toInt()
                ByteArray(count).also {
                    input.seek(offset)
                    input.readFully(it)
                }
            }
        }
    }

    /** Deletes take [id]: the journal first, so a crash in between leaves only audio, which [open] removes. */
    fun delete(id: RecordingId) {
        journal(id).delete()
        audio(id).delete()
    }

    private fun readJournal(id: RecordingId): JSONObject? {
        val file = journal(id)
        if (!file.isFile) return null
        return try {
            JSONObject(file.readText()).takeIf { it.optString("id") == id.value }
        } catch (e: JSONException) {
            null
        }
    }

    /** Replaces [id]'s journal atomically: a temporary file, synced, then renamed over it. */
    private fun write(id: RecordingId, entry: JSONObject) {
        val temporary = File(dir, "$id$JOURNAL$TEMPORARY")
        FileOutputStream(temporary).use { out ->
            out.write(entry.toString().toByteArray(Charsets.UTF_8))
            out.fd.sync()
        }
        if (!temporary.renameTo(journal(id))) {
            temporary.delete()
            throw IOException("cannot replace the journal of $id")
        }
    }

    /** Runs file work, reporting its failures as [RecorderException.Storage]. */
    private inline fun <T> storage(work: () -> T): T = try {
        work()
    } catch (e: IOException) {
        throw RecorderException.Storage(e)
    } catch (e: SecurityException) {
        throw RecorderException.Storage(e)
    }

    private companion object {
        const val AUDIO = ".aac"
        const val JOURNAL = ".json"
        const val TEMPORARY = ".tmp"
        const val RECORDING = "recording"
        const val FINISHED = "finished"
    }
}
