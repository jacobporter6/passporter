package dev.passporter.app

import android.content.Intent
import app.tauri.plugin.JSObject
import dev.passporter.notify.AgentId
import dev.passporter.notify.NoticeNotification

/**
 * What the Kotlin side asks the shell to show. The shell (Rust,
 * `apps/android/src/destination.rs` `Request`) builds every URL from the
 * saved bridge origin and ignores a request before a bridge is paired; this
 * side only names the screen.
 */
sealed interface ShellRequest {
    /** The request as the JSON the shell reads. */
    fun toJson(): JSObject

    /** A notification tap: [agent]'s screen on the bridge. */
    data class OpenAgent(val agent: AgentId) : ShellRequest {
        override fun toJson(): JSObject = kind("openAgent").apply { put("agent", agent.value) }
    }

    /** The launcher's *Bridge* shortcut: the bundled Bridge page. */
    data object ManageBridge : ShellRequest {
        override fun toJson(): JSObject = kind("manageBridge")
    }

    /** The bridge's page did not load: the Bridge page, saying so. */
    data object LoadFailed : ShellRequest {
        override fun toJson(): JSObject = kind("loadFailed")
    }

    companion object {
        /** Intent action of the *Bridge* shortcut (`res/xml/shortcuts.xml`). */
        const val ACTION_MANAGE_BRIDGE = "dev.passporter.app.MANAGE_BRIDGE"

        /**
         * What [intent] asks for, or null for any other intent (the launcher's
         * `MAIN`). `MainActivity` is exported, so any app can send these; the
         * worst they do is open an agent's screen or the Bridge page.
         */
        fun from(intent: Intent): ShellRequest? =
            NoticeNotification.agentToOpen(intent)?.let(::OpenAgent)
                ?: ManageBridge.takeIf { intent.action == ACTION_MANAGE_BRIDGE }

        private fun kind(kind: String): JSObject = JSObject().apply { put("kind", kind) }
    }
}
