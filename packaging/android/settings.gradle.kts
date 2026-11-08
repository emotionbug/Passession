// Android packaging for the Rust GUI. `cargo xtask build-gui android` drives
// this project with a pinned Gradle distribution; it is not a standalone app.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

plugins {
    id("com.android.application") version "9.5.1" apply false
    id("com.google.gms.google-services") version "4.4.4" apply false
    // Not applied: selects the Kotlin Gradle plugin behind AGP's built-in Kotlin
    // support. The default one AGP 9.5.1 brings calls Configuration.setVisible,
    // which Gradle 9.9 deprecates.
    id("org.jetbrains.kotlin.android") version "2.4.21" apply false
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "paseo-sessions"
include(":app")
