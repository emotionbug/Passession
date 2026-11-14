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

The upstream `text::tests::test` rasterization demo is removed. It registered no
`Sans` font, asserted no behavior, and saved PNGs to `/Users/ejpbruel/Desktop`.
The portable layout and atlas regression tests retain their bundled fixtures.

## Implicit font contour closure

`text/glyph_outline.rs` closes each outline contour at the next move or end,
and treats repeated explicit closes as idempotent. CFF2 outlines from Android's
variable Noto Sans CJK can omit the final close callback; the missing edge left
scanline winding nonzero and filled Korean glyphs with rectangles. The synthetic
regression checks closed versus implicit contours, an interior counter and the
exterior pixels without depending on a device font.

## Tabs and control characters

Upstream shapes control characters as themselves. No font maps them, so every
font of the family returns `.notdef` and the last one draws its box.
`src/text/shaper.rs` shapes each control character (`char::is_control`: C0, DEL,
C1) as a space instead, keeping its own cluster, so the shaped text, row
substrings and glyph count per byte are unchanged for selection, copy and find.
A TAB keeps the space advance as its unit and is flagged; all other controls
(the CR of CRLF, form feed, ESC, ...) get a zero advance. The right-to-left
placeholder reshaping keeps them blank as well.

`src/text/layouter.rs` resolves a TAB's advance when the glyph is placed: the
distance from its row-local x (including `first_row_indent_in_lpxs`) to the next
multiple of `TAB_STOP_IN_SPACES` (4) space advances. The word/grapheme fitter
measures candidates from the row position they would start at, so wrapping sees
the same widths; trailing tabs hang like other breakable whitespace.
`paseo_linebreak_tests` covers tab stops with cursor/selection geometry, the
zero-width controls and wrapping.

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

`DrawText` keeps its Rust-only page-area list before `DrawVars`. Native Rust
subclasses append shader instance fields after the base float tail; a trailing
`Vec` previously displaced their uploaded focus/hover values from the reflected
slots. The regression exercises Live reflection and the actual instance slice.

## Selection handle geometry

`DrawText::draw_abs_laidout` paints retained text at a scrolled origin without
reshaping it, keeping selection hit testing and rendered text on the same layout.
`Cx2d::end_pass_sized_turtle_with_visible_anchor` anchors handle decorations to
their owner's final alignment while escaping nested text-content clips.
Draw-list/window clipping still applies; a clipped caret hides its decoration.
The widget layer shares this mechanism across editable and read-only owners.
The overlay records its owner's position after applying each displacement, so
successive enclosing-turtle finalizations cannot apply the same scroll or
bottom-alignment shift repeatedly.

## Runtime font families

Applications can replace a font family's list after it was defined (system
fonts, user font preferences). `Fonts::redefine_font_family` (with
`Layouter::redefine_font_family` and `Loader::redefine_font_family`) installs
the new list, drops the loaded family and every cached UI layout, and advances
the font catalog revision. `AsyncLayout` already withholds completions for an
older catalog revision; the layout worker now remembers the family lists it was
given and redefines any family whose list changed in a newer catalog, instead
of only defining unseen families. `Fonts::font_family_definition` returns the
current list and `Fonts::get_or_load_font` exposes a font's metrics.
`FontFamily::font_family_id()` makes a DSL text style's family id public.
`Fonts::revision` exposes the catalog revision for retained widget layout caches.

`FontDefinition::variations` selects a variable-font instance with user-space
axis values (`(*b"wght", 600.0)`); `FontFace::from_data_index_and_variations`
applies them to the ttf-parser face before the rustybuzz face is built, so
outlines, metrics, advances and shaping all use that instance.
`from_data_and_index` keeps its signature (default instance). DSL fonts use no
variations. Collection faces (`index` > 0) were already supported.

A definition whose bytes do not parse at its face index, or an undefined font
id, no longer panics: `Loader::get_or_load_font` returns `None`, logs the
failure once and families skip that face (an all-failed family shapes nothing,
as an empty family did before). `paseo_font_tests` and the async-layout test
`a_family_redefinition_withholds_the_stale_result_and_relayouts_with_new_fonts`
cover redefinition, stale results and skipped faces with the existing widgets
fixtures. The variation and `.ttc` tests read system fonts named by
`MAKEPAD_TEST_VARIABLE_FONT` / `MAKEPAD_TEST_FONT_COLLECTION` (default: the
Ubuntu Sans variable font and Noto Sans CJK collection paths of Ubuntu) and
skip when the file is absent; no font resources are added.
