package dev.passporter.app

import android.app.Application
import android.content.Intent
import dev.passporter.notify.NoticeNotification
import dev.passporter.notify.agent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

@RunWith(RobolectricTestRunner::class)
class ShellRequestTest {
    private val context: Application = RuntimeEnvironment.getApplication()

    @Test
    fun aNotificationTapOpensItsAgent() {
        val id = agent("w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4")
        val tap = NoticeNotification.openAgent(context, id)
        assertEquals(ShellRequest.OpenAgent(id), ShellRequest.from(tap))
    }

    @Test
    fun theShortcutOpensTheBridgePage() {
        val activity = Robolectric.buildActivity(BridgeShortcutActivity::class.java).get()
        val forwarded = BridgeShortcutActivity.forward(activity)
        assertEquals(MainActivity::class.java.name, forwarded.component?.className)
        assertEquals(ShellRequest.ManageBridge, ShellRequest.from(forwarded))
    }

    @Test
    fun anyOtherIntentAsksForNothing() {
        val launcher = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
        assertNull(ShellRequest.from(launcher))
        val wrongAction = NoticeNotification.openAgent(context, agent("w1:p1")).setAction(Intent.ACTION_VIEW)
        assertNull(ShellRequest.from(wrongAction))
    }

    @Test
    fun anIntentRedeliveredFromRecentsAsksForNothing() {
        val tap = NoticeNotification.openAgent(context, agent("w1:p1"))
            .addFlags(Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY)
        assertNull(ShellRequest.from(tap))
    }

    /** The same JSON `destination.rs` `reads_what_the_kotlin_side_sends` reads. */
    @Test
    fun theWireFormIsWhatTheShellReads() {
        assertEquals(
            """{"kind":"openAgent","agent":"w1:p1~9ca09978-ef89"}""",
            ShellRequest.OpenAgent(agent("w1:p1~9ca09978-ef89")).toJson().toString(),
        )
        assertEquals("""{"kind":"manageBridge"}""", ShellRequest.ManageBridge.toJson().toString())
        assertEquals("""{"kind":"loadFailed"}""", ShellRequest.LoadFailed.toJson().toString())
    }

    @Test
    fun theShortcutGoesThroughItsOwnTask() {
        val shortcuts = DocumentBuilderFactory.newInstance().apply { isNamespaceAware = true }
            .newDocumentBuilder().parse(File("src/main/res/xml/shortcuts.xml"))
        val intent = shortcuts.getElementsByTagName("intent").item(0)
        val android = "http://schemas.android.com/apk/res/android"
        val attributes = intent.attributes
        assertEquals(BridgeShortcutActivity::class.java.name, attributes.getNamedItemNS(android, "targetClass").nodeValue)
        assertEquals(context.packageName, attributes.getNamedItemNS(android, "targetPackage").nodeValue)
    }
}
