package dev.passporter.app

import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import dev.passporter.notify.NotificationPermission

class MainActivity : TauriActivity() {
  private val notificationPermission = NotificationPermission(this)

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onResume() {
    super.onResume()
    notificationPermission.askIfDue()
  }
}
