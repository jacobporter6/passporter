package dev.passporter.app

import android.content.Intent
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import dev.passporter.device.NotificationHost
import dev.passporter.device.NotificationStatus
import dev.passporter.notify.NotificationPermission
import dev.passporter.notify.actions.Reconciler
import dev.passporter.push.NotificationSetup
import dev.passporter.push.PushRegistration

class MainActivity : TauriActivity(), NotificationHost {
  // One prompt at a time: push's distributor picker and the background
  // offer wait for the notification permission's answer.
  private val push: PushRegistration = PushRegistration.attachTo(
    this,
    promptsHeld = { notificationPermission.asking },
    onBuiltIn = { setup.offerBackgroundOnce() },
  )
  private val notificationPermission = NotificationPermission(this, onAnswered = { push.refresh() })
  private val setup = NotificationSetup(this, refresh = { push.refresh() })

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    // A notification tap that creates this activity in a live process is
    // not an onNewIntent: hand it to the shell with the rebuilt webview.
    ShellPlugin.created(this, savedInstanceState)
    super.onCreate(savedInstanceState)
    if (savedInstanceState == null) openSetupIfAsked(intent)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    openSetupIfAsked(intent)
  }

  override fun onWebViewCreate(webView: WebView) {
    ShellPlugin.wrap(this, webView)
  }

  override fun onResume() {
    super.onResume()
    notificationPermission.askIfDue()
    Reconciler.schedule(this)
  }

  override fun notificationStatus(): NotificationStatus = setup.status()

  override fun openNotificationSetup() = setup.open()

  /** The *Notifications* shortcut or the built-in transport's notification; not again from Recents. */
  private fun openSetupIfAsked(intent: Intent) {
    val fromHistory = intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0
    if (intent.action == NotificationSetup.ACTION && !fromHistory) setup.open()
  }
}
