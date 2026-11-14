package dev.passporter.app

import android.view.View
import android.widget.FrameLayout
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class SystemBarsTest {
    private val view = View(RuntimeEnvironment.getApplication()).apply {
        layoutParams = FrameLayout.LayoutParams(400, 800)
        SystemBars.fit(this)
    }

    private fun dispatch(bars: Insets, cutout: Insets = Insets.NONE, keyboard: Int = 0) {
        ViewCompat.dispatchApplyWindowInsets(view, WindowInsetsCompat.Builder()
            .setInsets(WindowInsetsCompat.Type.systemBars(), bars)
            .setInsets(WindowInsetsCompat.Type.displayCutout(), cutout)
            .setInsets(WindowInsetsCompat.Type.ime(), Insets.of(0, 0, 0, keyboard))
            .build())
    }

    private fun margins() = (view.layoutParams as FrameLayout.LayoutParams).let {
        listOf(it.leftMargin, it.topMargin, it.rightMargin, it.bottomMargin)
    }

    @Test
    fun scrollingCannotMoveTheViewportUnderTheClockOrNavigationBar() {
        dispatch(Insets.of(0, 28, 0, 24))
        view.scrollTo(0, 1000)
        assertEquals(listOf(0, 28, 0, 24), margins())
    }

    @Test
    fun rotationUsesTheLargerCutoutWithoutAccumulatingInsets() {
        dispatch(Insets.of(0, 28, 0, 24))
        repeat(2) { dispatch(Insets.of(0, 20, 16, 0), Insets.of(40, 0, 0, 0)) }
        assertEquals(listOf(40, 20, 16, 0), margins())
        dispatch(Insets.NONE)
        assertEquals(listOf(0, 0, 0, 0), margins())
    }

    @Test
    fun theKeyboardDoesNotAddASecondBottomInset() {
        dispatch(Insets.of(0, 28, 0, 24), keyboard = 320)
        assertEquals(listOf(0, 28, 0, 24), margins())
    }
}
