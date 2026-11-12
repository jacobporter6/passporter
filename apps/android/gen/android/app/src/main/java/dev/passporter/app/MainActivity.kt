package dev.passporter.app

import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import dev.passporter.notify.NotificationPermission
import dev.passporter.notify.actions.Reconciler
import dev.passporter.push.PushRegistration

class MainActivity : TauriActivity() {
  // One prompt at a time: push's distributor picker waits for the
  // notification permission's answer.
  private val push: PushRegistration = PushRegistration.attachTo(this, promptsHeld = { notificationPermission.asking })
  private val notificationPermission = NotificationPermission(this, onAnswered = { push.refresh() })

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    // A notification tap that creates this activity in a live process is
    // not an onNewIntent: hand it to the shell with the rebuilt webview.
    ShellPlugin.created(this, savedInstanceState)
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    ShellPlugin.wrap(this, webView)
  }

  override fun onResume() {
    super.onResume()
    notificationPermission.askIfDue()
    Reconciler.schedule(this)
  }
}
