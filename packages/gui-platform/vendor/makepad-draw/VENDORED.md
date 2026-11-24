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

## Font outline bounds and contour closure

`text/glyph_outline.rs` closes each outline contour at the next move or end,
and treats repeated explicit closes as idempotent. CFF2 outlines from Android's
variable Noto Sans CJK can omit the final close callback; the missing edge left
scanline winding nonzero and filled Korean glyphs with rectangles. The synthetic
regression checks closed versus implicit contours, an interior counter and the
exterior pixels without depending on a device font.

The raster bounds also include the emitted floating-point outline points and
curve control hulls. The parser's integer box truncates variable-font extrema
toward zero; clipping a negative fractional edge can leave the same winding
imbalance even in a closed contour. Fractional diagonal tests cover both
windings, and slash tests cover regular/semibold faces, with an optional
`MAKEPAD_TEST_VARIABLE_FONT` fixture at weights 400 and 600.

## Tabs and control characters

Upstream shapes control characters as themselves. No font maps them, so every
font of the family returns `.notdef` and the last one draws its box.
`src/text/shaper.rs` shapes each control character (`char::is_control`: C0, DEL,
C1) as a space instead, keeping its own cluster, so the shaped text, row
substrings and glyph count per byte are unchanged for selection, copy and find.
A TAB keeps the space advance as its unit and is flagged; all other controls
(the CR of CRLF, form feed, ESC, ...) get a zero advance.

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

UI and asynchronous font families share `FallbackRequests` through the font
catalog. Missing shaped glyphs enqueue their source grapheme's Unicode scalars;
the application resolves installed faces off-thread and extends the family.
Queue overflow leaves omitted scalars eligible for a later layout. Per-family
Unicode bitmaps prevent repeated negative lookups from cycling indefinitely.
A fresh queue on appearance changes permits previously unavailable glyphs.

The upstream RTL workaround replaced decreasing clusters with literal squares.
The shaper now retains contextual glyphs in logical-cluster order so discovery
and source selection refer to the original text. This removes forced missing
glyphs; it does not implement full bidi visual ordering.

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

## Retained list release

`DrawList2d::release` detaches a retained list and clears its paint and dirty
rect, so the next `begin_maybe` repaints instead of replaying. At `Overlay::end`
the platform draw-list pool checks each overlay's complete attachment chain to
the current window root, pruning released branches even when all internal
paint IDs are still current. Cached children follow a resubmitted ancestor in
paint order, including through ordinary draw lists. Pane overlays composite
before shell overlays and the window cursor; a nested overlay inherits its
ancestor's rank unless it explicitly uses a higher one. Composition reuses a
scratch buffer and does not move GPU resources.

## Retained viewport composition

`DrawText` honors draw-list shift and clip uniforms, like `DrawQuad` and
`DrawIcon`. All three evaluate local shape/texture coordinates before the
shift, so a retained viewport can scroll without moving or re-emitting any
instances. `DrawText::draw_areas` exposes the latest draw's glyph batches for
retained color updates across atlas pages.

The quad, text, and icon vertex implementations opt in to conservative
fragment bounds. `DrawColor` and first-party shaders that inherit these
vertex functions without replacing any of their callees inherit that
guarantee. The terminal's synthesized italic shader shears after clipping
and does not. Alignment edits mark both upload dirtiness and retained paint
revision; view transforms schedule a repaint of their owning pass.

## Standard rustybuzz, ttf-parser and PNG

`Cargo.toml` and `Cargo.toml.orig` replace `makepad-rustybuzz` 0.8.0, Makepad's
rustybuzz fork with its own `makepad-ttf-parser`, with the standard `rustybuzz`
0.20. Its `rustybuzz::ttf_parser` is the `ttf-parser` 0.25 this crate already
required directly, so faces, outlines and shaping use one parser shared with
makepad-vector and the GUI's resvg. `src/cx_draw.rs`, `src/text/{font,font_face,
glyph_outline,glyph_raster_image,shaper}.rs` and `paseo_font_tests.rs` drop the
`makepad_rustybuzz as rustybuzz` alias or name `rustybuzz::` directly.

`png` moves from 0.17 to the 0.18 the GUI uses. In `src/text/glyph_raster_image.rs`,
`Decoder::new` takes a `Cursor` over the glyph bytes, and `output_buffer_size`
now returns `Option`; a bitmap glyph whose decoded size does not fit `usize`
panics with a message, as other glyph PNG decode failures already do.

## Glyph-atlas dirty rectangles

Each atlas page accumulates a bounding box beginning at the first glyph added
since the last upload, not at `(0, 0)`. Taking that box resets it to empty.
Later uploads of a single glyph touch only its region, even on a partially
filled page; texture creation still initializes the full page.

## Cached lists that missed a full redraw

`Cx::redraw_all` (vendored makepad-platform) increments
`Cx::redraw_all_generation` besides flagging the next draw event. Each
`DrawList2d` remembers the generation it was last checked under, and
`Cx2d::will_redraw`/`will_redraw_check_axis` redraw a cached list whose
generation is older. Upstream, a list that was not drawn in the frame carrying
the flag (a view hidden behind a page or in a background tab) kept its retained
content, so it showed the previous theme's colors, font sizes or language when it
was next shown.
