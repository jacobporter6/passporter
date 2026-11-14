package dev.passporter.app

import android.view.View
import android.view.ViewGroup.MarginLayoutParams
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updateLayoutParams

/** Keep the entire web viewport inside system bars and camera cutouts, including
 * sticky headers and fixed close buttons. WebView safe-area CSS is not reliable
 * across Android versions. IME resizing remains the window's responsibility.
 */
internal object SystemBars {
    fun fit(view: View) {
        ViewCompat.setOnApplyWindowInsetsListener(view) { content, insets ->
            val bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
            )
            content.updateLayoutParams<MarginLayoutParams> {
                setMargins(bars.left, bars.top, bars.right, bars.bottom)
            }
            insets
        }
        ViewCompat.requestApplyInsets(view)
    }
}
