package dev.passporter.app

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.junit.Assert.assertEquals
import org.junit.Test
import org.w3c.dom.Document
import org.w3c.dom.Element

/**
 * Guards the app's transport and backup policy (docs/development/android.md):
 * no plain HTTP, system CAs only, nothing backed up or transferred, and the
 * manifest actually uses each config.
 * Gradle runs unit tests from the module directory, so paths are relative
 * to `app/`.
 */
class SecurityConfigTest {
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

    private fun Element.childElements(tag: String): List<Element> {
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
    }

    @Test
    fun nothingIsBackedUpOrTransferred() {
        val application = parse("src/main/AndroidManifest.xml").elements("application").single()
        assertEquals("false", application.getAttributeNS(androidNs, "allowBackup"))
        assertEquals(
            "@xml/data_extraction_rules",
            application.getAttributeNS(androidNs, "dataExtractionRules"),
        )
        assertEquals("@xml/backup_rules", application.getAttributeNS(androidNs, "fullBackupContent"))

        val everyDomain = listOf("root", "file", "database", "sharedpref", "external")
        val rules = parse("src/main/res/xml/data_extraction_rules.xml")
        for (section in listOf("cloud-backup", "device-transfer")) {
            val element = rules.elements(section).single()
            assertEquals(section, emptyList<Element>(), element.childElements("include"))
            assertEquals(
                section,
                everyDomain,
                element.childElements("exclude").map { it.getAttribute("domain") },
            )
        }
        val legacy = parse("src/main/res/xml/backup_rules.xml")
        assertEquals(emptyList<Element>(), legacy.elements("include"))
        assertEquals(everyDomain, legacy.elements("exclude").map { it.getAttribute("domain") })
    }
}
