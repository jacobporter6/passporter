package dev.passporter.push

import org.junit.Assert.assertEquals
import org.junit.Test

/** What the built-in transport falls back to once it stops ([Fallback.decide]). */
class FallbackTest {
    @Test
    fun aSavedDistributorTakesOverWhenTheBridgeIsReady() {
        assertEquals(Fallback.Register("vapid"), Fallback.decide("io.heckel.ntfy") { Readiness.Register("vapid") })
    }

    @Test
    fun aSavedDistributorTheBridgeIsNotReadyForIsTold() {
        for (readiness in listOf(Readiness.NotPaired, Readiness.BridgeOff, Readiness.Later)) {
            assertEquals(Fallback.Tell(readiness), Fallback.decide("io.heckel.ntfy") { readiness })
        }
    }

    @Test
    fun withoutASavedDistributorTheBridgeIsNotAskedAndItIsTold() {
        assertEquals(Fallback.Tell(null), Fallback.decide(null) { error("the bridge was asked") })
    }
}
