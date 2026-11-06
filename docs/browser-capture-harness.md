# Native capture boundary

The product no longer has an embedded browser, browser automation, Electron or Chromium. The former browser guest capture harness is not a native-client test and has no current product command. This filename remains only so existing documentation links resolve; it does not describe a retained browser feature.

For client screenshots, launch the real Rust/Makepad application with its staged resources against an isolated Rust daemon and capture its native window. Record the actual platform, viewport, revision and state shown. A website, retained Flutter client, browser replica or generated image does not prove the Rust client rendered that state.

Use [testing](testing.md) for native smoke boundaries and recorded evidence, and [mobile testing](mobile-testing.md) for device-specific interaction. Historical screenshots establish only their recorded build. Linux captures do not imply Windows/macOS/Android/iOS execution.

External links open outside Paseo. There is no browser-profile persistence, guest WebContents ownership, CDP screenshot path, embedded automation toolset or fallback webview to test in the native product. The unused website is removed; browser tests of a user's project must not be reported as application coverage.
