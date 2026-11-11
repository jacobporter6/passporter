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
    testImplementation("junit:junit:4.13.2")
}

// NetworkSecurityConfigTest reads these at run time; declare them so Gradle
// reruns the tests when they change.
tasks.withType<Test>().configureEach {
    inputs.files("src/main/AndroidManifest.xml", "src/main/res/xml/network_security_config.xml")
        .withPropertyName("transportPolicy")
        .withPathSensitivity(PathSensitivity.RELATIVE)
}

apply(from = file("tauri.build.gradle.kts"))
