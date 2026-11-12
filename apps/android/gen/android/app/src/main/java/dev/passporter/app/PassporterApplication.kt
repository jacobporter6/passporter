package dev.passporter.app

import android.app.Application
import dev.passporter.device.DeviceAuth

/**
 * The app's process: registers what must hear every pairing change for as
 * long as the process lives, whichever component started it (an activity,
 * a push, a queued answer): [PairingReset], after every pairing and forget.
 */
class PassporterApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        // On the thread that paired or forgot (never the main thread), before it returns.
        DeviceAuth.get(this).addPairingListener { PairingReset.run(this) }
    }
}
