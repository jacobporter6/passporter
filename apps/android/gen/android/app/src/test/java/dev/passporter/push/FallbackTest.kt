package dev.passporter.push

import org.junit.Assert.assertEquals
import org.junit.Test

/** What the built-in transport falls back to once it stops ([Fallback.decide]), and keeps until it connects ([Interim]). */
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
        assertEquals(Fallback.NoDistributor, Fallback.decide(null) { error("the bridge was asked") })
    }

    @Test
    fun untilTheStreamConnectsTheSavedDistributorKeepsARegistration() {
        assertEquals("vapid", Interim.vapidKey(connected = false, "io.heckel.ntfy") { Readiness.Register("vapid") })
    }

    @Test
    fun onceTheStreamConnectsNoExternalRegistrationIsMade() {
        assertEquals(null, Interim.vapidKey(connected = true, "io.heckel.ntfy") { error("the bridge was asked") })
    }

    @Test
    fun beforeTheStreamConnectsNoPickerAndNoRegistrationTheBridgeIsNotReadyFor() {
        assertEquals(null, Interim.vapidKey(connected = false, null) { error("the bridge was asked") })
        for (readiness in listOf(Readiness.NotPaired, Readiness.BridgeOff, Readiness.Later)) {
            assertEquals(null, Interim.vapidKey(connected = false, "io.heckel.ntfy") { readiness })
        }
    }
}
