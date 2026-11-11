package dev.passporter.device

import java.io.File
import java.nio.file.Files
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class BridgeOriginTest {
    @Test
    fun readsWhatTheShellWrites() {
        // The shell writes `{origin}\n` (apps/android/src/store.rs).
        assertEquals("https://bridge.example.org", BridgeOrigin.parse("https://bridge.example.org\n").toString())
        assertEquals("https://10.0.0.2:8443", BridgeOrigin.parse("https://10.0.0.2:8443").toString())
        assertEquals("https://xn--brcke-lva.example", BridgeOrigin.parse("https://xn--brcke-lva.example").toString())
    }

    @Test
    fun refusesAnythingElse() {
        for (bad in listOf(
            "", "http://bridge.example.org", "https://me@bridge.example.org", "https://bridge.example.org/x",
            "https://bridge.example.org?x", "https://bridge.example.org#x", "bridge.example.org", "https://",
        )) {
            assertNull(bad, BridgeOrigin.parse(bad))
        }
    }

    @Test
    fun resolvesOnlyAbsolutePaths() {
        val origin = checkNotNull(BridgeOrigin.parse("https://bridge.example.org"))
        assertEquals("https://bridge.example.org/v1/auth/pair", origin.resolve("/v1/auth/pair"))
        for (bad in listOf("v1", "//evil.example/x", "@evil.example/", "")) {
            assertThrows(bad, IllegalArgumentException::class.java) { origin.resolve(bad) }
        }
    }

    @Test
    fun theSavedOriginIsReadFromTheDataDirectory() {
        val dir = Files.createTempDirectory("origin").toFile()
        assertNull(BridgeOrigin.saved(dir))
        File(dir, BridgeOrigin.FILE_NAME).writeText("https://bridge.example.org\n")
        assertEquals("https://bridge.example.org", BridgeOrigin.saved(dir).toString())
        dir.deleteRecursively()
    }

    /**
     * Native code and the webview must agree on the bridge, so both read the
     * shell's one file: pin the name against the shell's source. Gradle runs
     * unit tests from the module directory (`plugins/device/android`).
     */
    @Test
    fun theFileNameIsTheShells() {
        val store = File("../../../src/store.rs").readText()
        assertTrue(store.contains("""pub const FILE_NAME: &str = "${BridgeOrigin.FILE_NAME}";"""))
        assertTrue(
            "the shell keeps the file in Tauri's app data directory (Context.getDataDir())",
            File("../../../src/shell.rs").readText().contains("Store::in_dir(&app.path().app_data_dir()?)"),
        )
    }
}
