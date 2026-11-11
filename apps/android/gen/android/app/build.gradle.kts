import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release signing comes only from the environment (docs/development/android.md).
// `just android-build release` refuses to start without all four; a Gradle run
// without them leaves the release APK unsigned.
fun env(name: String): String? = providers.environmentVariable(name).orNull?.takeIf { it.isNotEmpty() }
val releaseKeystore = env("PASSPORTER_ANDROID_KEYSTORE")
val releaseKeystorePassword = env("PASSPORTER_ANDROID_KEYSTORE_PASSWORD")
val releaseKeyAlias = env("PASSPORTER_ANDROID_KEY_ALIAS")
val releaseKeyPassword = env("PASSPORTER_ANDROID_KEY_PASSWORD")

android {
    compileSdk = 37
    // Pinned to what deploy/android/setup.sh installs, so Gradle never downloads build tools.
    buildToolsVersion = "37.0.0"
    namespace = "dev.passporter.app"
    defaultConfig {
        applicationId = "dev.passporter.app"
        minSdk = 26
        targetSdk = 37
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (releaseKeystore != null && releaseKeystorePassword != null &&
            releaseKeyAlias != null && releaseKeyPassword != null
        ) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = releaseKeystorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }
    buildTypes {
        getByName("debug") {
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            signingConfig = signingConfigs.findByName("release")
            optimization {
               enable = true
            }
            proguardFiles(
                *fileTree(".") {
                  include("**/*.pro")
                  exclude("build/**")
                }.files.toTypedArray()
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        buildConfig = true
    }
    testOptions {
        // Robolectric tests (dev.passporter.notify) read the app's resources.
        unitTests.isIncludeAndroidResources = true
    }
    sourceSets {
        // The bridge's Web Push vector (WebPushVectorTest), from where its Rust test pins it.
        getByName("test").resources.directories.add("../../../../../crates/bridge/src/push/fixtures")
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    // Notifications (dev.passporter.notify); the version appcompat resolves anyway.
    implementation("androidx.core:core:1.13.1")
    // UnifiedPush (dev.passporter.push): distributor registration, Web Push
    // keys in the Android Keystore, RFC 8291 decryption.
    implementation("org.unifiedpush.android:connector:3.3.5")
    // The native outbox of answers from notifications (dev.passporter.notify.actions).
    implementation("androidx.work:work-runtime:2.11.2")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.17")
    testImplementation("androidx.work:work-testing:2.11.2")
    // WebPushVectorTest calls the connector's Tink decrypter; the version the connector uses.
    testImplementation("com.google.crypto.tink:tink:1.23.0")
}

tasks.withType<Test>().configureEach {
    // Robolectric's Android 15+ sandboxes reach a JDK internal.
    jvmArgs("--add-opens=java.base/jdk.internal.access=ALL-UNNAMED")
    // SecurityConfigTest reads these at run time; declare them so Gradle
    // reruns the tests when they change.
    inputs.files("src/main/AndroidManifest.xml", "src/main/res/xml")
        .withPropertyName("transportPolicy")
        .withPathSensitivity(PathSensitivity.RELATIVE)
}

apply(from = file("tauri.build.gradle.kts"))
