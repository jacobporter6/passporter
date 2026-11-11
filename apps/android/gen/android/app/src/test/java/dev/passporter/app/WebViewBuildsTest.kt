package dev.passporter.app

import org.junit.Assert.assertEquals
import org.junit.Test

class WebViewBuildsTest {
    @Test
    fun theFirstWebViewIsNoRebuild() {
        var rebuilt = 0
        val builds = WebViewBuilds(onRebuilt = { rebuilt++ })
        builds.built()
        assertEquals(0, rebuilt)
    }

    @Test
    fun everyLaterOneIs() {
        var rebuilt = 0
        val builds = WebViewBuilds(onRebuilt = { rebuilt++ })
        repeat(3) { builds.built() }
        assertEquals(2, rebuilt)
    }
}
