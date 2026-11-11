package dev.passporter.app

import android.net.Uri
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class LinkPolicyTest {
    private fun tapped(url: String) =
        LinkPolicy.opensInBrowser(Uri.parse(url), mainFrame = true, gesture = true, redirect = false)

    @Test
    fun webAddressesTheUserTappedOpenInTheBrowser() {
        for (url in listOf("https://example.com/", "http://example.com/a?b#c", "HTTPS://Example.COM/")) {
            assertTrue(url, tapped(url))
        }
    }

    @Test
    fun otherSchemesNeverDo() {
        for (url in listOf(
            "intent://scan/#Intent;scheme=zxing;package=com.example;end",
            "file:///sdcard/page.html",
            "content://com.example.provider/file",
            "javascript:alert(1)",
            "data:text/html,<p>hi</p>",
            "market://details?id=com.example",
            "tel:123",
            "mailto:someone@example.com",
            "blob:https://example.com/1b4f",
            "about:blank",
            "https:///no-host",
            "https://tauri.localhost/index.html",
            "https://TAURI.localhost/bridge.html",
        )) {
            assertFalse(url, tapped(url))
        }
    }

    @Test
    fun onlyATopLevelTapThatIsNoRedirect() {
        val url = Uri.parse("https://example.com/")
        assertFalse(LinkPolicy.opensInBrowser(url, mainFrame = false, gesture = true, redirect = false))
        assertFalse(LinkPolicy.opensInBrowser(url, mainFrame = true, gesture = false, redirect = false))
        assertFalse(LinkPolicy.opensInBrowser(url, mainFrame = true, gesture = true, redirect = true))
    }

    @Test
    fun theBridgePageIsTheBundledOne() {
        assertTrue(LinkPolicy.isBridgePage(Uri.parse("https://tauri.localhost/bridge.html")))
        assertTrue(LinkPolicy.isBridgePage(Uri.parse("https://tauri.localhost/bridge.html?unreachable")))
        assertFalse(LinkPolicy.isBridgePage(Uri.parse("https://tauri.localhost/index.html")))
        assertFalse(LinkPolicy.isBridgePage(Uri.parse("https://bridge.example.org/bridge.html")))
    }

    @Test
    fun onlyBridgePagesAreRemote() {
        assertTrue(LinkPolicy.isRemote(Uri.parse("https://bridge.example.org/agent/w1:p1")))
        assertFalse(LinkPolicy.isRemote(Uri.parse("https://tauri.localhost/index.html")))
        assertFalse(LinkPolicy.isRemote(Uri.parse("about:blank")))
        assertFalse(LinkPolicy.isRemote(Uri.parse("data:text/html,x")))
    }
}
