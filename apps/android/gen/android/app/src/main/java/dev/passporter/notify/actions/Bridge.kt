package dev.passporter.notify.actions

import android.content.Context
import dev.passporter.device.DeviceAuth
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId
import dev.passporter.device.HttpResponse
import dev.passporter.device.Method
import dev.passporter.device.Scope

/**
 * An authenticated request to the paired bridge: [DeviceAuth.send] with an
 * app-scope token in the app ([DeviceBridge]), a fake in tests. Blocking;
 * throws [DeviceAuthException] when there is no answer or no session.
 */
fun interface Bridge {
    /**
     * Sends [method] [path] with [json] as its body, if any: only as
     * [device] when given ([DeviceAuth.send]), as whatever the app is
     * paired as otherwise.
     */
    fun send(method: Method, path: String, json: String?, device: DeviceId?): HttpResponse
}

/** [Bridge] over the app's device identity, with an app-scope session. */
internal class DeviceBridge(private val context: Context) : Bridge {
    override fun send(method: Method, path: String, json: String?, device: DeviceId?): HttpResponse =
        DeviceAuth.get(context).send(Scope.APP, method, path, json, device)
}
