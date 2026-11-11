package dev.passporter.app

import android.graphics.Bitmap
import android.net.Uri
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import java.util.Locale

/**
 * Wraps wry's `RustWebViewClient` ([inner]) to learn what wry does not pass
 * to the shell: whether a refused navigation was a tap, and whether a page
 * failed to load.
 *
 * Every method [inner] overrides is passed to it first, unchanged (the
 * shell's navigation guard answers in [shouldOverrideUrlLoading]); a test
 * (`ShellWebViewClientTest`) fails if wry starts overriding one this class
 * does not pass on. The one exception: a page's own navigation to the
 * Bridge page is refused here, before the guard. On top:
 *
 * - a link the guard refused opens in the browser when [LinkPolicy.opensInBrowser]
 *   says so. The webview itself still never leaves the bridge;
 * - a main-frame load that fails, off the bundled pages ([LinkPolicy.isRemote]),
 *   calls [onLoadFailed]: no network, the bridge down, or a 5xx from its
 *   proxy. A PWA the service worker has cached does not fail offline;
 * - history never holds a bundled page beside another page
 *   ([LinkPolicy.mixesBundledPages]): Back and Forward skip the guard.
 */
class ShellWebViewClient(
    private val inner: WebViewClient,
    private val onLoadFailed: () -> Unit,
    private val openInBrowser: (Uri) -> Unit,
) : WebViewClient() {
    override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? =
        inner.shouldInterceptRequest(view, request)

    override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
        // Only the shell opens the Bridge page (its own loads never come
        // through here), so no page can use the guard's allowance for it.
        if (LinkPolicy.isBridgePage(request.url)) return true
        val refused = inner.shouldOverrideUrlLoading(view, request)
        if (refused && LinkPolicy.opensInBrowser(request.url, request.isForMainFrame, request.hasGesture(), request.isRedirect)) {
            openInBrowser(request.url)
        }
        return refused
    }

    override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) =
        inner.onPageStarted(view, url, favicon)

    override fun onPageFinished(view: WebView, url: String) {
        inner.onPageFinished(view, url)
        // History never holds a bundled page beside another one, so no
        // bridge page can go Back or Forward to a page with the app
        // commands (history skips the guard), and Back from the Bridge page
        // leaves the app instead of reloading a bridge page that failed.
        val history = view.copyBackForwardList()
        val urls = (0 until history.size).mapNotNull { history.getItemAtIndex(it)?.url }
        if (LinkPolicy.mixesBundledPages(urls.map(Uri::parse))) view.clearHistory()
    }

    override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
        // wry retries one case itself: ERROR_CONNECT when another request
        // was intercepted since this one (its fix for redirects to a custom
        // protocol; in-flight API calls trigger it too). Its retry can take
        // the Bridge page's own request, but it reloads the failed URL,
        // which fails again with nothing intercepted in between and lands
        // here once more: the Bridge page follows, after a flicker.
        inner.onReceivedError(view, request, error)
        if (request.isForMainFrame && LinkPolicy.isRemote(request.url)) onLoadFailed()
    }

    override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
        inner.onReceivedHttpError(view, request, response)
        if (request.isForMainFrame && response.statusCode >= 500 && LinkPolicy.isRemote(request.url)) onLoadFailed()
    }
}

/** The decisions [ShellWebViewClient] adds to the shell's navigation guard. */
object LinkPolicy {
    /** Where Tauri serves the bundled pages (`navigation.rs` `LOCAL_HOST`). */
    const val LOCAL_HOST = "tauri.localhost"

    private val WEB_SCHEMES = setOf("https", "http")

    /**
     * Whether a navigation to [url] that the guard refused opens in the
     * browser instead: a top-level ([mainFrame]) navigation the person
     * started ([gesture], such as *Open* on the PWA's link confirm), not a
     * [redirect], to a web address. Never another scheme (`intent:`, `file:`, `javascript:`,
     * `content:`, ...), never a frame, a redirect or a script on its own, and
     * never the bundled pages.
     */
    fun opensInBrowser(url: Uri, mainFrame: Boolean, gesture: Boolean, redirect: Boolean): Boolean {
        val host = host(url)
        return mainFrame && gesture && !redirect && scheme(url) in WEB_SCHEMES && !host.isNullOrEmpty() && host != LOCAL_HOST
    }

    /** The bundled Bridge page's path (`destination.rs` `BRIDGE_PAGE`). */
    const val BRIDGE_PAGE_PATH = "/bridge.html"

    /** Whether [url] is the bundled Bridge page, with or without a query. */
    fun isBridgePage(url: Uri): Boolean =
        scheme(url) == "https" && host(url) == LOCAL_HOST && url.path == BRIDGE_PAGE_PATH

    /** Whether [url] is one of the bundled pages. */
    fun isBundled(url: Uri): Boolean = scheme(url) == "https" && host(url) == LOCAL_HOST

    /** Whether a history of [urls] holds a bundled page beside another page. */
    fun mixesBundledPages(urls: List<Uri>): Boolean = urls.size > 1 && urls.any(::isBundled)

    /** Whether [url] is a remote page (the bridge's), not a bundled one. */
    fun isRemote(url: Uri): Boolean = scheme(url) == "https" && host(url) != LOCAL_HOST

    private fun scheme(url: Uri): String? = url.scheme?.lowercase(Locale.ROOT)

    private fun host(url: Uri): String? = url.host?.lowercase(Locale.ROOT)
}
