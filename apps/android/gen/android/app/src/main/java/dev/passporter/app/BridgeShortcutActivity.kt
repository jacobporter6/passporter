package dev.passporter.app

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import dev.passporter.push.NotificationSetup

/**
 * The launcher's shortcuts land here, and are passed on to [MainActivity]:
 * *Bridge* as [ShellRequest.ACTION_MANAGE_BRIDGE], *Notifications* as
 * [NotificationSetup.ACTION].
 *
 * Android starts a static shortcut with `FLAG_ACTIVITY_CLEAR_TASK`, which
 * would destroy a running `MainActivity` and with it Tauri's webview. This
 * activity has a task of its own (an empty `taskAffinity`), so only that
 * task is cleared, and [MainActivity] (`singleTask`) gets the request in
 * `onNewIntent` when it runs, or starts with it when it does not.
 */
class BridgeShortcutActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        startActivity(forward(this, intent.action))
        finish()
    }

    companion object {
        /** The intent this activity starts for a shortcut of [action]. */
        fun forward(activity: Activity, action: String?): Intent =
            Intent(activity, MainActivity::class.java)
                .setAction(if (action == NotificationSetup.ACTION) action else ShellRequest.ACTION_MANAGE_BRIDGE)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    }
}
