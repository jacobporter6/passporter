package dev.passporter.app

import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import dev.passporter.notify.NotificationPermission
import dev.passporter.push.PushRegistration

class MainActivity : TauriActivity() {
  private val notificationPermission = NotificationPermission(this)

  init {
    PushRegistration.attachTo(this)
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    ShellPlugin.wrap(this, webView)
  }

  override fun onResume() {
    super.onResume()
    notificationPermission.askIfDue()
  }
}
