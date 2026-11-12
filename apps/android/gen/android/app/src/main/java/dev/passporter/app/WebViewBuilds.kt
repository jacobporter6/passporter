package dev.passporter.app

/**
 * Counts the webviews wry builds in this process, and tells the shell
 * ([send]) about every one after the first: [ShellRequest.WebViewRebuilt],
 * then what the intent that created its activity asked for, if anything.
 *
 * wry builds a new webview, in a new `MainActivity`, in the same process:
 *
 * - when Android re-creates the activity, for a configuration change the
 *   manifest does not list (a new font or display size, for one) or after
 *   destroying it in the background;
 * - when a notification tap (or the launcher) starts a new activity in a
 *   live process: after Back finished the last one (Android 8 to 11), or a
 *   swipe from Recents while the dictation service kept the process.
 *
 * Each time it loads the first page it was given at launch again. Once a
 * bridge is paired the shell refuses the bundled Connect page, so a
 * webview that started there would stay blank: the shell answers
 * [ShellRequest.WebViewRebuilt] with the bridge (`destination.rs`
 * `FirstPage::after_rebuild`). What the new activity was asked to show is
 * sent right after it, so the shell opens the tapped agent last, over
 * whatever the rebuild showed: the intent that created it ([creating]), or
 * one Android delivered to it before its webview was built ([requested]:
 * a re-created activity gets the tap as a new intent). The intent that
 * starts a process is not sent here: the shell asks for it when it starts
 * (`ShellPlugin.watch`). A process Android ended and restores from Recents
 * starts a new count.
 *
 * Not thread-safe: call it on the UI thread only.
 */
class WebViewBuilds(private val send: (ShellRequest) -> Unit) {
    private var count = 0

    /** Whether an activity is being created and its webview is not built yet. */
    private var building = false

    /** What the intent that created that activity asked for. */
    private var created: ShellRequest? = null

    /** The latest request delivered to that activity meanwhile. */
    private var held: ShellRequest? = null

    /**
     * Call when an activity is created, before its webview is built, with
     * what its intent asked for ([ShellRequest.from]), or `null` when it is
     * restored from a saved state (its intent is old news).
     */
    fun creating(request: ShellRequest?) {
        building = true
        created = request
        held = null
    }

    /**
     * Shows [request], a later intent's: now, or once the activity being
     * created has its webview.
     */
    fun requested(request: ShellRequest) {
        if (building) held = request else send(request)
    }

    /** Call once for each webview wry builds, once it is set up. */
    fun built() {
        count += 1
        val request = held ?: created.takeIf { count > 1 }
        building = false
        created = null
        held = null
        if (count > 1) send(ShellRequest.WebViewRebuilt)
        request?.let(send)
    }
}
