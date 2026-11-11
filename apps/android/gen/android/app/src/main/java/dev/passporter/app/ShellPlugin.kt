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
 * The shell's own Kotlin side: the Tauri plugin `passporter-shell`
 * (`apps/android/src/native.rs`). Only Rust calls it: the plugin has no
 * permissions, so Tauri refuses its commands to every page.
 *
 * It tells the shell what to show ([ShellRequest]): the intent that started
 * the app (answered by [watch]), each later one ([onNewIntent], which Tauri
 * forwards from `MainActivity`), and failed loads ([wrap]). It restarts the
 * app ([restart]).
 *
 * Tauri makes one instance per process. wry may build a new webview (and a
 * new activity) in the same process, so what outlives one of them (the
 * channel to the shell) lives in the companion, and each webview is wrapped
 * as it is made (`MainActivity.onWebViewCreate`).
 */
@TauriPlugin
class ShellPlugin(private val activity: Activity) : Plugin(activity) {
    private var launchTaken = false

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
        val app = activity.applicationContext
        app.startActivity(Intent.makeRestartActivityTask(ComponentName(app, MainActivity::class.java)))
        Runtime.getRuntime().exit(0)
    }

    companion object {
        private const val TAG = "PassporterShell"

        /** Where requests go once the shell [watch]es; process-wide. */
        @Volatile
        private var requests: Channel? = null

        /**
         * Wraps [webView]'s client in a [ShellWebViewClient] once wry has set
         * it: call it from `onWebViewCreate`, which runs just before wry sets
         * the client, and it runs on the next turn of the UI loop, before
         * the first page loads (wry posts that load after this).
         */
        fun wrap(activity: Activity, webView: WebView) {
            webView.post {
                val inner = webView.webViewClient
                if (inner is ShellWebViewClient) return@post
                webView.webViewClient = ShellWebViewClient(
                    inner = inner,
                    onLoadFailed = { send(ShellRequest.LoadFailed) },
                    openInBrowser = { openInBrowser(activity, it) },
                )
            }
        }

        private fun send(request: ShellRequest) {
            requests?.send(request.toJson())
        }

        private fun openInBrowser(activity: Activity, url: Uri) {
            val view = Intent(Intent.ACTION_VIEW, url).addCategory(Intent.CATEGORY_BROWSABLE)
            try {
                activity.startActivity(view)
            } catch (e: ActivityNotFoundException) {
                Log.w(TAG, "no app opens web links", e)
            }
        }
    }
}
