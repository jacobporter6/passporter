package dev.passporter.device

import android.app.Activity
import android.util.Log
import androidx.webkit.WebViewFeature
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * The device identity, as the PWA sees it (`plugin:device|<command>`), plus
 * one call for the shell. The ACL lets only the paired bridge's origin call
 * `status`, `pair`, `session` and `forget` (`permissions/default.toml`);
 * `webviewFeatures` has no permission, so only Rust reaches it.
 *
 * The webview only ever gets an app-scope token: a car-scope token is for
 * native code ([DeviceAuth]) alone.
 */
@TauriPlugin
class DevicePlugin(private val activity: Activity) : Plugin(activity) {
    private val auth: DeviceAuth by lazy { DeviceAuth.get(activity) }

    /** `{webMessageListener}`: whether the WebView labels each frame's messages with its own origin. */
    @Command
    fun webviewFeatures(invoke: Invoke) {
        val supported = WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)
        if (!supported) Log.w(TAG, "this WebView lacks WEB_MESSAGE_LISTENER; native IPC stays off")
        invoke.resolve(JSObject().apply { put("webMessageListener", supported) })
    }

    /** `{paired, deviceId?}`. */
    @Command
    fun status(invoke: Invoke) = work(invoke, emptySet()) {
        val status = auth.status()
        JSObject().apply {
            put("paired", status.paired)
            status.deviceId?.let { put("deviceId", it.value) }
        }
    }

    /** `{code, name?}` → `{deviceId}`. The code is normalized already (`XXXXXXXX`). */
    @Command
    fun pair(invoke: Invoke) = work(invoke, setOf("code", "name")) { args ->
        val code = (args.opt("code") as? String)?.let(PairingCode::parse)
            ?: throw DeviceAuthException.InvalidInput("code")
        val name = when (val raw = args.opt("name")) {
            null, org.json.JSONObject.NULL -> null
            is String -> DeviceName.parse(raw) ?: throw DeviceAuthException.InvalidInput("name")
            else -> throw DeviceAuthException.InvalidInput("name")
        }
        JSObject().apply { put("deviceId", auth.pair(code, name).value) }
    }

    /**
     * A fresh app-scope token, shaped like `protocol::SessionGrant`:
     * `{token, expiresInSecs, deviceName?}`. Always fresh, so a token the
     * bridge just refused is never handed back.
     */
    @Command
    fun session(invoke: Invoke) = work(invoke, emptySet()) {
        val token = auth.freshToken(Scope.APP)
        JSObject().apply {
            put("token", token.value)
            put("expiresInSecs", auth.secondsLeft(token))
            token.deviceName?.let { put("deviceName", it) }
        }
    }

    /** Forgets the pairing here. Resolves `null`. */
    @Command
    fun forget(invoke: Invoke) = work(invoke, emptySet()) {
        auth.forget()
        null
    }

    /**
     * Runs [body] on the worker with the command's arguments, which may hold
     * only [allowed] keys, and answers with its result or its failure
     * (`{message, code}`; codes from [DeviceAuthException.code]).
     */
    private fun work(invoke: Invoke, allowed: Set<String>, body: (JSObject) -> JSObject?) {
        worker.execute {
            try {
                val args = invoke.getArgs()
                val unknown = args.keys().asSequence().firstOrNull { it !in allowed }
                if (unknown != null) throw DeviceAuthException.InvalidInput("argument $unknown")
                val result = body(args)
                if (result == null) invoke.resolve() else invoke.resolve(result)
            } catch (e: DeviceAuthException) {
                invoke.reject(e.message, e.code)
            } catch (e: Exception) {
                Log.e(TAG, "device command ${invoke.command} failed", e)
                invoke.reject("the device plugin failed", "internal")
            }
        }
    }

    private companion object {
        const val TAG = "PassporterDevice"

        /**
         * Network and Keystore work stays off the thread Tauri calls in on.
         * One pool for the process, so a recreated activity leaks no thread;
         * [DeviceAuth] serializes the work itself.
         */
        val worker: ExecutorService = Executors.newCachedThreadPool()
    }
}
