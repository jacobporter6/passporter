package dev.passporter.device

/** How agent news reaches the phone, as the PWA's settings show it: no endpoints, no keys. */
class NotificationStatus(
    /** Whether news arrives now (the status light is green). */
    val connected: Boolean,
    /** One sentence saying how, or what is wrong. */
    val message: String,
)

/**
 * What the app's activity offers the PWA about notifications
 * ([DevicePlugin.notificationStatus], [DevicePlugin.notificationSetup]).
 * The plugin is a library the app depends on, so the app implements this.
 */
interface NotificationHost {
    /** The status now. Called on a worker thread. */
    fun notificationStatus(): NotificationStatus

    /** Opens the app's own notification setup. Called on the main thread. */
    fun openNotificationSetup()
}
