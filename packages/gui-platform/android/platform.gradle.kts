// Gradle dependencies of the paseo-gui-platform Java glue. Applied by
// packaging/android/app/build.gradle.kts (string configuration names: this is
// an applied script, not a plugin-typed build file).
dependencies {
    // FileProvider and NotificationCompat (per-notification silent flag on API 24+).
    "implementation"("androidx.core:core:1.13.1")
    // Firebase Messaging 24 issues the FCM registration token Expo exchanges.
    // Provisioning stays optional: without google-services.json FirebaseApp
    // does not initialize and push is reported as unavailable.
    "implementation"(platform("com.google.firebase:firebase-bom:33.16.0"))
    "implementation"("com.google.firebase:firebase-messaging")
}
