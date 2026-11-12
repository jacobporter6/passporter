package dev.passporter.notify.actions

import android.content.Context
import android.net.Uri
import android.util.Log
import dev.passporter.device.DeviceAuth
import dev.passporter.device.DeviceAuthException
import dev.passporter.device.DeviceId

/** What the app is paired as now, as far as it can tell ([pairedDevice]). */
sealed interface PairedAs {
    /** Paired as [id]. */
    data class Device(
        /** The device. */
        val id: DeviceId,
    ) : PairedAs

    /** Not paired. */
    data object Nothing : PairedAs

    /** It could not tell (a busy Keystore just after boot, say): try again later. */
    data object Unknown : PairedAs

    /** The device, if paired. */
    val device: DeviceId? get() = (this as? Device)?.id

    /** Whether the app is known to be paired as anything but [device] (or not at all). */
    fun isNot(device: DeviceId?): Boolean = this != Unknown && this.device != device
}

/**
 * What the app is paired as now ([DeviceAuth.status]). Blocking (a
 * Keystore look-up; it never waits for a sign-in). Tests replace it, and
 * put [currentDevice] back.
 */
internal var pairedDevice: (Context) -> PairedAs = ::currentDevice

/** [pairedDevice] in the app: [DeviceAuth.status]. */
internal fun currentDevice(context: Context): PairedAs =
    try {
        DeviceAuth.get(context).status().deviceId?.let(PairedAs::Device) ?: PairedAs.Nothing
    } catch (e: DeviceAuthException) {
        Log.w("PassporterNotify", "could not read the pairing: ${e.code}")
        PairedAs.Unknown
    }

/**
 * The device a notification's action intents were made for, stamped on
 * their data as the query parameter `device`
 * (`passporter://action/reply/w1:p1?device=dev-…`). Like the rest of the
 * data, an app holding a mutable pending intent cannot change it.
 *
 * Every answer from a notification, the shade's ([OutboxJob.device]) or
 * the car's (`dev.passporter.car.CarJob.device`), carries the device it was
 * made for, and is sent only while the app is still paired as that device:
 * an answer to one bridge never reaches another one paired since (agent
 * ids such as `w1:p1` repeat across bridges), nor the same bridge after a
 * new pairing.
 */
object DeviceStamp {
    private const val PARAM = "device"

    /** [uri] stamped with [device]. */
    fun stamp(uri: Uri, device: DeviceId): Uri = uri.buildUpon().appendQueryParameter(PARAM, device.value).build()

    /** The device [uri] is stamped with, or `null` if none (or not a device id). */
    fun of(uri: Uri?): DeviceId? = uri?.getQueryParameter(PARAM)?.let(DeviceId::parse)
}
