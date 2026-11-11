package dev.passporter.app

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.junit.Assert.assertEquals
import org.junit.Test
import org.w3c.dom.Document
import org.w3c.dom.Element

/**
 * Guards the app's transport policy (docs/development/android.md): no
 * plain HTTP, system CAs only, and the manifest actually uses the config.
 * Gradle runs unit tests from the module directory, so paths are relative
 * to `app/`.
 */
class NetworkSecurityConfigTest {
    private val androidNs = "http://schemas.android.com/apk/res/android"

    private fun parse(path: String): Document =
        DocumentBuilderFactory.newInstance()
            .apply { isNamespaceAware = true }
            .newDocumentBuilder()
            .parse(File(path))

    private fun Document.elements(tag: String): List<Element> {
        val nodes = getElementsByTagName(tag)
        return (0 until nodes.length).map { nodes.item(it) as Element }
    }

    private val config = parse("src/main/res/xml/network_security_config.xml")

    @Test
    fun cleartextIsOffEverywhere() {
        val base = config.elements("base-config").single()
        assertEquals("false", base.getAttribute("cleartextTrafficPermitted"))
        assertEquals(
            "no domain may opt back into cleartext",
            emptyList<Element>(),
            config.elements("domain-config"),
        )
    }

    @Test
    fun onlySystemCertificatesAreTrusted() {
        val sources = config.elements("certificates").map { it.getAttribute("src") }
        assertEquals(listOf("system"), sources)
        assertEquals(emptyList<Element>(), config.elements("debug-overrides"))
    }

    @Test
    fun theManifestUsesTheConfigAndNoCleartextFlag() {
        val application = parse("src/main/AndroidManifest.xml").elements("application").single()
        assertEquals(
            "@xml/network_security_config",
            application.getAttributeNS(androidNs, "networkSecurityConfig"),
        )
        assertEquals("", application.getAttributeNS(androidNs, "usesCleartextTraffic"))
        assertEquals("false", application.getAttributeNS(androidNs, "allowBackup"))
    }
}
