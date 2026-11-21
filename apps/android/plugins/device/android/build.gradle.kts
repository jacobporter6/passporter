import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// The device plugin's Kotlin library. tauri-build includes it in the app's
// Gradle project as `:tauri-plugin-device` (gen/android/tauri.settings.gradle),
// next to `:tauri-android`; other plugins that need an authenticated token
// depend on it the same way.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.passporter.device"
    compileSdk = 37
    buildToolsVersion = "37.0.0"

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

dependencies {
    implementation(project(":tauri-android"))
    implementation("androidx.webkit:webkit:1.14.0")
    testImplementation("junit:junit:4.13.2")
    // org.json is part of Android, but its JVM unit-test stub throws.
    testImplementation("org.json:json:20250517")
}
