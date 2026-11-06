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
    id("com.android.application") version "9.1.1" apply false
    id("com.google.gms.google-services") version "4.4.4" apply false
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "paseo-sessions"
include(":app")
