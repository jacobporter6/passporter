package dev.passporter.app

/**
 * Counts the webviews wry builds in this process, and calls [onRebuilt] for
 * every one after the first.
 *
 * wry builds a new webview in the same process when Android re-creates
 * `MainActivity` for a configuration change the manifest does not list (a
 * new font or display size, for one), and loads the first page it was given
 * at launch again. Once a bridge is paired the shell refuses the bundled
 * Connect page, so a webview that started there would stay blank: the shell
 * answers [ShellRequest.WebViewRebuilt] with the bridge (`destination.rs`
 * `FirstPage::after_rebuild`). A process Android ended and restores from
 * Recents starts a new count.
 *
 * Not thread-safe: call [built] on the UI thread only.
 */
class WebViewBuilds(private val onRebuilt: () -> Unit) {
    private var count = 0

    /** Call once for each webview wry builds, once it is set up. */
    fun built() {
        count += 1
        if (count > 1) onRebuilt()
    }
}
