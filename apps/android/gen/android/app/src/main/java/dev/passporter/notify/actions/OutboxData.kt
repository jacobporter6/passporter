package dev.passporter.notify.actions

import androidx.work.Data
import dev.passporter.notify.AgentId
import dev.passporter.notify.PendingId

/** An [OutboxJob] as WorkManager input [Data]. */
internal object OutboxData {
    private const val KEY = "key"
    private const val ACTION = "action"
    private const val AGENT = "agent"
    private const val PENDING = "pending"
    private const val TEXT = "text"
    private const val NAME = "name"
    private const val CREATED_AT = "createdAt"
    private const val ATTEMPTS = "attempts"

    /** The input of [job], which has had [attempts] attempts. */
    fun write(job: OutboxJob, attempts: Int): Data =
        Data.Builder()
            .putString(KEY, job.key)
            .putString(ACTION, job.request.action.wire)
            .putString(AGENT, job.request.agent.value)
            .putString(PENDING, job.request.pending?.value)
            .putString(TEXT, job.text)
            .putString(NAME, job.name)
            .putLong(CREATED_AT, job.createdAt)
            .putInt(ATTEMPTS, attempts)
            .build()

    /** The job in [data], or `null` if it is not one (an older app's, say). */
    fun read(data: Data): OutboxJob? {
        val action = data.getString(ACTION)?.let(NoticeAction::fromWire) ?: return null
        val agent = data.getString(AGENT)?.let(AgentId::parse) ?: return null
        val pending = data.getString(PENDING)?.let { PendingId.parse(it) ?: return null }
        if ((pending != null) != action.answersDialog) return null
        val text = data.getString(TEXT)
        if ((text != null) != action.takesText) return null
        return OutboxJob(
            key = data.getString(KEY) ?: return null,
            request = ActionRequest(action, agent, pending),
            text = text,
            name = data.getString(NAME) ?: return null,
            createdAt = data.getLong(CREATED_AT, 0),
        )
    }

    /** Attempts made before this run's ([read]'s job). */
    fun attempts(data: Data): Int = data.getInt(ATTEMPTS, 0)
}
