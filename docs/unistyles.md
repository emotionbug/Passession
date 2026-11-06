# Native styling

The client uses Makepad Rust widgets and shaders. This filename is retained for documentation links; Unistyles, CSS and React Native styles are not runtime APIs.

`packages/gui/src/theme.rs` owns the runtime palette, typography and theme epoch. `theme_makepad.rs` supplies the font chains and stock-widget theme; `common/{widgets,paint}.rs` supplies shared controls and drawing. Custom primitives read the current palette when drawing. Apply changes to stock DSL widgets through the theme's epoch-aware path: DSL colors loaded once do not follow a preference change automatically.

Theme changes must preserve host/session identity, editor drafts, scroll position and active subscriptions. Rebuilding presentation is not permission to restart provider work or reset navigation. Use semantic colors and native text styles, respect text scaling and contrast, and keep loading/error/disabled states readable in both themes.

Compact layouts must account for safe areas, keyboard insets and touch targets. Hover-only affordances need a visible or long-press alternative. Desktop and touch behavior are input/layout concerns, not evidence of a browser target.

Measure actual Makepad event/layout/draw behavior on the affected platform. Keep redraws tied to input, data, caret and bounded animation activity; a theme change must not introduce an idle polling loop. See [design](design.md), [development](development.md), and [testing](testing.md).
