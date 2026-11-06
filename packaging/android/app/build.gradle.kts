import java.util.Properties

plugins {
    id("com.android.application")
}

// Sessions Firebase credentials are optional, staged by xtask and must belong to sh.paseo.sessions.
if (file("google-services.json").isFile) {
    apply(plugin = "com.google.gms.google-services")
}

fun required(name: String): String =
    providers.gradleProperty(name).orNull?.takeIf { it.isNotBlank() }
        ?: error("Missing -P$name; build with cargo xtask build-gui android")

// Manifest, Java glue, resources and Gradle dependencies of the platform crate.
val platformDir = rootProject.file("../../packages/gui-platform/android")
apply(from = platformDir.resolve("platform.gradle.kts"))

val signingProperties = Properties().apply {
    val propertiesFile = rootProject.file("key.properties")
    if (propertiesFile.isFile) propertiesFile.inputStream().use { load(it) }
}
fun signingValue(environment: String, property: String): String? =
    System.getenv(environment)?.takeIf { it.isNotBlank() }
        ?: signingProperties.getProperty(property)?.takeIf { it.isNotBlank() }

val releaseStore = signingValue("ANDROID_KEYSTORE_PATH", "storeFile")
val releaseStorePassword = signingValue("ANDROID_KEYSTORE_PASSWORD", "storePassword")
val releaseAlias = signingValue("ANDROID_KEY_ALIAS", "keyAlias")
val releaseKeyPassword = signingValue("ANDROID_KEY_PASSWORD", "keyPassword")
val signingValues = listOf(releaseStore, releaseStorePassword, releaseAlias, releaseKeyPassword)
require(signingValues.all { it == null } || signingValues.all { it != null }) {
    "Provide all Android release signing values, not a partial signing configuration."
}
val hasReleaseSigning = releaseStore != null
val releaseRequested = gradle.startParameter.taskNames.any { it.contains("release", ignoreCase = true) }
if (releaseRequested && !hasReleaseSigning) {
    require(System.getenv("PASEO_ANDROID_REQUIRE_RELEASE_SIGNING") != "1") {
        "A production Android signing key is required for this release."
    }
    logger.warn("Building a locally installable, debug-signed Sessions release; not for production publishing.")
}

android {
    namespace = "sh.paseo.sessions"
    // AGP 9.1.1 resolves the android-37.0 platform only with an explicit minor level.
    compileSdk = 37
    compileSdkMinor = 0
    ndkVersion = "28.2.13676358"
    ndkPath = required("paseoNdkPath")

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "sh.paseo.sessions"
        // Same floor as the Flutter client. The native library links the API 24
        // NDK sysroot; xtask rejects libraries that need newer system libraries.
        minSdk = 24
        targetSdk = 36
        versionCode = maxOf(13000, required("paseoVersionCode").toInt())
        versionName = required("paseoVersionName")
        ndk.abiFilters.addAll(required("paseoAbis").split(","))
    }

    sourceSets {
        getByName("main") {
            manifest.srcFile(platformDir.resolve("AndroidManifest.xml"))
            java.srcDirs(rootProject.file("makepad/java"), platformDir.resolve("java"))
            res.srcDirs(rootProject.file("res"), platformDir.resolve("res"))
            assets.srcDirs(required("paseoAssets"))
            jniLibs.srcDirs(required("paseoJniLibs"))
        }
    }

    signingConfigs {
        // Keep local upgrade compatibility with the existing Sessions Android app.
        getByName("debug") {
            storeFile = file("debug.keystore")
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
        if (hasReleaseSigning) {
            create("release") {
                storeFile = rootProject.file(releaseStore!!)
                storePassword = releaseStorePassword
                keyAlias = releaseAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        release {
            // Native code finds the Java glue by name through JNI; never shrink or rename it.
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName(if (hasReleaseSigning) "release" else "debug")
        }
    }

    lint {
        // The vendored upstream makepad Java is not lint-clean (e.g. Bluetooth
        // permission checks for APIs the app never reaches).
        checkReleaseBuilds = false
    }
}
