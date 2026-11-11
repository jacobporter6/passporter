package dev.passporter.app

import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import dev.passporter.notify.NotificationPermission

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    NotificationPermission.requestIfConnected(this)
  }
}
