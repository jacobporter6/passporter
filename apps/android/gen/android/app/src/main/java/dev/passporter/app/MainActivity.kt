package dev.passporter.app

import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import dev.passporter.notify.NotificationPermission
import dev.passporter.notify.actions.Reconciler

class MainActivity : TauriActivity() {
  private val notificationPermission = NotificationPermission(this)

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onResume() {
    super.onResume()
    notificationPermission.askIfDue()
    Reconciler.schedule(this)
  }
}
