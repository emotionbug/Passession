# Notifications

Notifications belong to `packages/gui-platform`: `src/mobile` owns push registration and token lifecycle; `src/android` plus `android/java/sh/paseo/sessions` owns Android delivery; `src/ios` owns APNs/native iOS callbacks; `src/desktop` owns desktop delivery and activation. `notification_policy.rs` owns shared policy. Platform events carry explicit targets to the Rust shell; platform code does not select another pane's global route.

## Identity and delivery

A notification target identifies the host and session. Resolve it against saved hosts and current daemon data before opening; do not select an equal session/path on another host or create a replacement session when the target is unavailable. Wait for application initialization and host readiness, deduplicate delivery, and expose stale/unreachable-target failures.

The native mobile implementation uses Firebase messaging on Android and APNs directly on iOS (the iOS app does not link Firebase), plus native local notifications. Both register their device token through the existing push service's Expo-compatible token exchange, and Android keeps Expo-formatted FCM application data. That service compatibility is not an Expo/Metro application runtime or a web client.

OS-rendered background notifications and locally presented foreground notifications have different ownership. Avoid duplicate alerts. Background handler errors must remain available to the foreground UI rather than being silently discarded. Notification permission is a user choice, not a daemon requirement.

## Configuration and privacy

Android/iOS credentials are optional and must match the Sessions application identity. Source builds do not include production push credentials or store provisioning. Do not claim push delivery works solely because a client compiles or a token is stored. Tokens and notification payloads may expose private host/session data; keep diagnostics scoped and redact credentials.

## Verification

Exercise permission denial/grant, foreground delivery, background delivery, terminated-app launch, duplicate taps, reconnect, stale targets and Back on the actual OS/device. Linux native client tests do not establish Android/iOS notification behavior. See [mobile testing](mobile-testing.md), [Android](android.md), and [testing](testing.md).
