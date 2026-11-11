package dev.passporter.push

import androidx.activity.ComponentActivity
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner

/** Attaches like MainActivity does, whose own launch needs the Rust library. */
@RunWith(RobolectricTestRunner::class)
class PushRegistrationTest {
    /** Attaches in its constructor, before it has a context, as MainActivity does. */
    class AttachingActivity : ComponentActivity() {
        init {
            PushRegistration.attachTo(this)
        }
    }

    @Test
    fun attachingInTheConstructorSurvivesTheActivitysLife() {
        // The push thread's work fails quietly here (no Keystore): what
        // matters is that nothing on the activity's side throws.
        Robolectric.buildActivity(AttachingActivity::class.java).use { controller ->
            controller.setup().pause().resume().pause().stop().destroy()
        }
    }
}
