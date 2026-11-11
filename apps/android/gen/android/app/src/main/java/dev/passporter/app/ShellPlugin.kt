package dev.passporter.app

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Intent
import android.net.Uri
import android.util.Log
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Channel
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

/** The arguments of [ShellPlugin.watch]. */
@InvokeArg
class WatchArgs {
    /** Where [ShellPlugin] sends later [ShellRequest]s. */
    lateinit var channel: Channel
}

/**
 * The shell's own Kotlin side: the Tauri plugin `shell`
 * (`apps/android/src/native.rs`). Only Rust calls it: the plugin has no
 * permissions, so Tauri refuses its commands to every page.
 *
 * It tells the shell what to show ([ShellRequest]): the intent that started
 * the app (answered by [watch]) and each later one ([onNewIntent], which
 * Tauri forwards from `MainActivity`), and failed loads. It opens refused
 * links in the browser ([ShellWebViewClient]) and restarts the app ([restart]).
 */
@TauriPlugin
// One instance per process, holding the activity Tauri created it with: the
// shell has one activity, its config changes keep the webview, and when it
// is destroyed Tauri's event loop ends with it (no second webview to wrap).
class ShellPlugin(private val activity: Activity) : Plugin(activity) {
    private var requests: Channel? = null
    private var launchTaken = false

    override fun load(webView: WebView) {
        // Tauri loads plugins once wry has set its client on the webview,
        // and before the first page loads.
        webView.webViewClient = ShellWebViewClient(
            inner = webView.webViewClient,
            onLoadFailed = { send(ShellRequest.LoadFailed) },
            openInBrowser = ::openInBrowser,
        )
    }

    /** `{channel}` → `{launch?}`: what the starting intent asked for, once. */
    @Command
    fun watch(invoke: Invoke) {
        requests = invoke.parseArgs(WatchArgs::class.java).channel
        val launch = if (launchTaken) null else ShellRequest.from(activity.intent)
        launchTaken = true
        invoke.resolve(JSObject().apply { launch?.let { put("launch", it.toJson()) } })
    }

    override fun onNewIntent(intent: Intent) {
        ShellRequest.from(intent)?.let(::send)
    }

    /**
     * Restarts the app in a new process, which opens on the Connect page now
     * that the shell has forgotten the bridge. Only a new process drops the
     * old bridge's IPC grant: Tauri cannot revoke a capability.
     */
    @Command
    fun restart(invoke: Invoke) {
        // TODO(T32c): forget the native pairing here (DeviceAuth.forget) once
        // the device plugin is on the branch.
        invoke.resolve()
        val main = Intent.makeRestartActivityTask(ComponentName(activity, MainActivity::class.java))
        activity.startActivity(main)
        Runtime.getRuntime().exit(0)
    }

    private fun send(request: ShellRequest) {
        requests?.send(request.toJson())
    }

    private fun openInBrowser(url: Uri) {
        val view = Intent(Intent.ACTION_VIEW, url).addCategory(Intent.CATEGORY_BROWSABLE)
        try {
            activity.startActivity(view)
        } catch (e: ActivityNotFoundException) {
            Log.w(TAG, "no app opens web links", e)
        }
    }

    private companion object {
        const val TAG = "PassporterShell"
    }
}
