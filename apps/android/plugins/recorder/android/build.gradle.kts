import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// The recorder plugin's Kotlin library. tauri-build includes it in the app's
// Gradle project as `:tauri-plugin-recorder` (gen/android/tauri.settings.gradle),
// next to `:tauri-android`. Its manifest (the foreground service and its
// permissions) merges into the app's.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.passporter.recorder"
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

    testOptions {
        // Robolectric tests (the service's notification) read the library's resources.
        unitTests.isIncludeAndroidResources = true
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

dependencies {
    implementation(project(":tauri-android"))
    // Notification and foreground-service helpers; the version the app resolves anyway.
    implementation("androidx.core:core:1.13.1")
    testImplementation("junit:junit:4.13.2")
    // org.json is part of Android, but its JVM unit-test stub throws.
    testImplementation("org.json:json:20250517")
    testImplementation("org.robolectric:robolectric:4.17")
}

tasks.withType<Test>().configureEach {
    // Robolectric's Android 15+ sandboxes reach a JDK internal.
    jvmArgs("--add-opens=java.base/jdk.internal.access=ALL-UNNAMED")
}
