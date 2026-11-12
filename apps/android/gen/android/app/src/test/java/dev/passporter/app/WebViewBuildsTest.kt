package dev.passporter.app

import dev.passporter.notify.agent
import org.junit.Assert.assertEquals
import org.junit.Test

class WebViewBuildsTest {
    private val sent = mutableListOf<ShellRequest>()
    private val builds = WebViewBuilds(send = { sent += it })
    private val tap = ShellRequest.OpenAgent(agent("w1:p2"))

    @Test
    fun theFirstWebViewIsNoRebuild() {
        builds.built()
        assertEquals(emptyList<ShellRequest>(), sent)
    }

    @Test
    fun everyLaterOneIs() {
        repeat(3) { builds.built() }
        assertEquals(List(2) { ShellRequest.WebViewRebuilt }, sent)
    }

    /** The shell asks for the launch tap itself (`ShellPlugin.watch`): not sent twice. */
    @Test
    fun theTapThatStartsTheProcessIsLeftToTheShell() {
        builds.creating(tap)
        builds.built()
        assertEquals(emptyList<ShellRequest>(), sent)
    }

    /** A tap that creates a new activity in a live process opens its agent, after the rebuild. */
    @Test
    fun aTapThatCreatesALaterActivityFollowsTheRebuild() {
        builds.built()
        builds.creating(tap)
        builds.built()
        assertEquals(listOf(ShellRequest.WebViewRebuilt, tap), sent)
        // Sent once: a later rebuild (a font size change) does not open it again.
        builds.built()
        assertEquals(listOf(ShellRequest.WebViewRebuilt, tap, ShellRequest.WebViewRebuilt), sent)
    }

    @Test
    fun anActivityCreatedWithoutARequestOnlyRebuilds() {
        builds.built()
        builds.creating(null)
        builds.built()
        assertEquals(listOf(ShellRequest.WebViewRebuilt), sent)
    }

    /** A re-created activity gets the tap as a new intent, before its webview is built. */
    @Test
    fun aTapDeliveredBeforeTheWebViewIsBuiltFollowsTheRebuild() {
        builds.built()
        builds.creating(null)
        builds.requested(tap)
        assertEquals(emptyList<ShellRequest>(), sent)
        builds.built()
        assertEquals(listOf(ShellRequest.WebViewRebuilt, tap), sent)
    }

    @Test
    fun aTapOnALiveActivityIsSentAtOnce() {
        builds.built()
        builds.requested(tap)
        assertEquals(listOf(tap), sent)
    }
}
