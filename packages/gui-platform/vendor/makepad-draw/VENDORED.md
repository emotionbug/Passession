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
