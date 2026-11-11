package dev.passporter.app

import android.app.Application
import android.net.Uri
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.TestWebResourceError
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import java.lang.reflect.Method
import java.lang.reflect.Modifier

@RunWith(RobolectricTestRunner::class)
class ShellWebViewClientTest {
    private val context: Application = RuntimeEnvironment.getApplication()
    private val view = WebView(context)

    private class Request(
        url: String,
        private val mainFrame: Boolean = true,
        private val gesture: Boolean = true,
        private val redirect: Boolean = false,
    ) : WebResourceRequest {
        private val uri = Uri.parse(url)
        override fun getUrl(): Uri = uri
        override fun isForMainFrame() = mainFrame
        override fun isRedirect() = redirect
        override fun hasGesture() = gesture
        override fun getMethod() = "GET"
        override fun getRequestHeaders(): Map<String, String> = emptyMap()
    }

    /** Stands in for wry's client: refuses what [refuse] says, records calls. */
    private class Inner(private val refuse: (Uri) -> Boolean) : WebViewClient() {
        val calls = mutableListOf<String>()
        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
            calls += "override ${request.url}"
            return refuse(request.url)
        }
        override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
            calls += "intercept ${request.url}"
            return null
        }
        override fun onPageStarted(view: WebView, url: String, favicon: android.graphics.Bitmap?) {
            calls += "started $url"
        }
        override fun onPageFinished(view: WebView, url: String) {
            calls += "finished $url"
        }
        override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
            calls += "error ${request.url}"
        }
        override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
            calls += "http ${response.statusCode} ${request.url}"
        }
    }

    private val bridge = "https://bridge.example.org/"
    private val inner = Inner { it.host != "bridge.example.org" }
    private val opened = mutableListOf<Uri>()
    private var failures = 0
    private val client = ShellWebViewClient(inner, onLoadFailed = { failures++ }, openInBrowser = { opened += it })

    private fun response(status: Int) = WebResourceResponse("text/html", "utf-8", status, "status", emptyMap(), null)

    @Test
    fun theGuardStillDecides() {
        assertFalse(client.shouldOverrideUrlLoading(view, Request("${bridge}agent/w1:p1")))
        assertTrue(client.shouldOverrideUrlLoading(view, Request("https://example.com/")))
        assertTrue(client.shouldOverrideUrlLoading(view, Request("intent://x#Intent;end")))
        assertEquals(
            listOf("override ${bridge}agent/w1:p1", "override https://example.com/", "override intent://x#Intent;end"),
            inner.calls,
        )
    }

    @Test
    fun noPageMayOpenTheBridgePage() {
        for (page in listOf("https://tauri.localhost/bridge.html", "https://tauri.localhost/bridge.html?unreachable")) {
            assertTrue(page, client.shouldOverrideUrlLoading(view, Request(page)))
        }
        assertEquals("refused before the guard", emptyList<String>(), inner.calls)
        assertEquals(emptyList<Uri>(), opened)
    }

    @Test
    fun aRefusedLinkTheUserTappedOpensInTheBrowser() {
        client.shouldOverrideUrlLoading(view, Request("https://example.com/docs"))
        client.shouldOverrideUrlLoading(view, Request("${bridge}settings"))
        client.shouldOverrideUrlLoading(view, Request("javascript:alert(1)"))
        client.shouldOverrideUrlLoading(view, Request("https://example.com/redirected", redirect = true))
        client.shouldOverrideUrlLoading(view, Request("https://example.com/by-script", gesture = false))
        client.shouldOverrideUrlLoading(view, Request("https://example.com/frame", mainFrame = false))
        assertEquals(listOf(Uri.parse("https://example.com/docs")), opened)
    }

    @Test
    fun everythingElseIsPassedOn() {
        val page = Request(bridge)
        assertEquals(null, client.shouldInterceptRequest(view, page))
        client.onPageStarted(view, bridge, null)
        client.onPageFinished(view, bridge)
        assertEquals(listOf("intercept $bridge", "started $bridge", "finished $bridge"), inner.calls)
    }

    @Test
    fun aBridgePageThatFailsToLoadSaysSo() {
        client.onReceivedError(view, Request(bridge), TestWebResourceError())
        client.onReceivedHttpError(view, Request("${bridge}agent/w1:p1"), response(502))
        assertEquals(2, failures)
        assertEquals(listOf("error $bridge", "http 502 ${bridge}agent/w1:p1"), inner.calls)
    }

    @Test
    fun subresourcesBundledPagesAndClientErrorsDoNot() {
        client.onReceivedError(view, Request("${bridge}app.wasm", mainFrame = false), TestWebResourceError())
        client.onReceivedHttpError(view, Request("${bridge}v1/agents", mainFrame = false), response(503))
        client.onReceivedHttpError(view, Request("${bridge}missing"), response(404))
        client.onReceivedError(view, Request("https://tauri.localhost/index.html"), TestWebResourceError())
        assertEquals(0, failures)
        assertEquals(4, inner.calls.size)
    }

    /**
     * wry's generated client: every method it overrides must be one this
     * wrapper passes on, or wry's handling (the navigation guard, Tauri's
     * asset protocol, IPC's page tracking) would be skipped.
     */
    @Test
    fun everyMethodWryOverridesIsPassedOn() {
        val overrides = { cls: Class<*> ->
            cls.declaredMethods
                .filter { !it.isSynthetic && !Modifier.isStatic(it.modifiers) && overridesWebViewClient(it) }
                .map { it.name to it.parameterTypes.toList() }
                .toSet()
        }
        val wry = overrides(RustWebViewClient::class.java)
        assertTrue("wry overrides the guard's method", ("shouldOverrideUrlLoading" to listOf(WebView::class.java, WebResourceRequest::class.java)) in wry)
        val missing = wry - overrides(ShellWebViewClient::class.java)
        assertTrue("not passed on to wry: $missing", missing.isEmpty())
    }

    private fun overridesWebViewClient(method: Method): Boolean =
        runCatching { WebViewClient::class.java.getDeclaredMethod(method.name, *method.parameterTypes) }.isSuccess
}
