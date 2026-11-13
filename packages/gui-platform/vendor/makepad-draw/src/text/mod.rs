//pub mod builtins;
pub mod async_layout;
pub use async_layout::{collect_retired_layouts, AsyncLayout};
mod source_index;
pub mod color;
pub mod font;
pub mod font_atlas;
pub mod font_face;
pub mod font_family;
pub mod fonts;
pub mod geom;
pub mod glyph_outline;
pub mod glyph_raster_image;
pub mod image;
pub mod intern;
pub mod layouter;
pub mod loader;
#[cfg(test)]
mod paseo_font_tests;
pub mod num;
pub mod rasterizer;
pub mod sdfer;
pub mod selection;
pub mod shaper;
pub mod slice;
pub mod substr;

