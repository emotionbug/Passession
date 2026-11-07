# Vendored makepad-draw 1.0.0

Source: published crates.io archive `makepad-draw-1.0.0.crate`, SHA-256
`de6d007a4364deadfe250732712516c7f9d1e31ebb9484c9051fa6c41bd0ff03`.
The root workspace patches this crate and excludes the vendor directory. The
archive's Cargo.lock and .cargo_vcs_info.json are omitted. The published archive
omits a license file; LICENSE retains Makepad's upstream MIT license, identical
to the existing makepad-platform vendor license. The manifest declares
MIT OR Apache-2.0.

## Line breaking

`src/text/layouter.rs` uses Unicode line-break opportunities (UAX #14) rather
than word boundaries (UAX #29). Word boundaries incorrectly allow punctuation
to start a new line. The existing newline handling and oversized-word grapheme
fallback remain in place.

Trailing breakable whitespace hangs outside the fit width. It remains in the
shaped text, row substrings and glyphs so byte-indexed selection, copy and find
keep referring to the original text. Nonbreaking spaces do not hang. Word
fitting skips the untrimmed-width estimate because it can reject a fitting word
with trailing spaces; grapheme fitting keeps the upstream estimate.

`paseo_linebreak_tests` covers punctuation, hanging spaces with cursor byte
positions, and nonbreaking spaces using the adjacent widgets crate's existing
Liberation Mono fixture. No font resources are added.

## Retained asynchronous layout

`text::AsyncLayout` runs the existing exact layouter with an independent CPU font
loader/shaper and immutable font-byte snapshots. UI `Rc<Font>` values and GPU
atlas state never cross threads. Results are hydrated back to UI fonts in bounded
glyph/row batches and become visible only as a complete revision; newer requests
do not splice new source text into old glyph geometry.

The fitter probes nearby line-break opportunities instead of repeatedly shaping
half of the remaining paragraph. Consuming a boundary no longer shifts every
remaining entry. Unicode breaking and shaping still use the original algorithms;
the optimization does not insert artificial paragraph boundaries.

`text/source_index.rs` retains source fingerprints, word boundaries, row lookup
and conservative glyph visibility ranges with each immutable layout. Painting
uses final viewport coordinates to avoid emitting/rasterizing offscreen glyphs,
including horizontally scrolled lines, without reordering glyphs or losing
source-indexed selection. The widgets crate supplies the final-position list
pass; preliminary `PortalList` coordinates are not a valid culling boundary.

Retiring a large UI layout queues its rows rather than synchronously releasing
every glyph. `text::collect_retired_layouts()` drains at most 4096 glyphs and
128 rows and reports remaining work. Worker-owned layouts dispose on their
worker. The application owns the UI continuation: an initial signal followed by
one-shot 60 Hz timers while work remains, independent of drawing or the lifetime
of the original row/window. Atlas preparation does not drive retirement.

## Stable glyph pages and overlay depth

`src/text/{font_atlas,rasterizer,fonts}.rs` retain existing glyph coordinates when
an atlas fills. Rasterization allocates another fixed-size page rather than
dropping a glyph and resetting UVs still referenced by retained draw lists.
Each grayscale/color page has its own texture; only unreferenced extra pages
can be recycled. `Texture::is_unique` is provided by the adjacent platform patch.
The atlas tests cover overflow pixel/coordinate preservation, safe page reuse
and oversized glyph rejection.

`src/shader/draw_text.rs` splits instances at page changes without appending
returning pages behind intervening batches. Multi-page text retains one aligned
hit/focus rectangle and the instance areas needed for live style updates.
Glyph depth stays at the authored draw depth: per-glyph accumulation previously
let long text paint above later overlay backgrounds and icons.
`src/cx_draw.rs` uploads all live pages without the old atlas-reset redraw loop;
the text debug exporter explicitly selects page zero.

## Selection handle geometry

`DrawText::draw_abs_laidout` paints retained text at a scrolled origin without
reshaping it, keeping selection hit testing and rendered text on the same layout.
`Cx2d::end_pass_sized_turtle_with_visible_anchor` anchors handle decorations to
their owner's final alignment while escaping nested text-content clips.
Draw-list/window clipping still applies; a clipped caret hides its decoration.
The widget layer shares this mechanism across editable and read-only owners.
