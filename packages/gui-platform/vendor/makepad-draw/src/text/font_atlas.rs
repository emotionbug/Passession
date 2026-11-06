use {
    super::{
        font::{FontId, GlyphId},
        geom::{Point, Rect, Size},
        image::{Image, Bgra, Subimage, SubimageMut, R},
        num::Zero,
    },
    std::{collections::HashMap, fs::File, io::BufWriter, path::Path, slice},
};

#[derive(Clone, Debug)]
pub struct FontAtlas<T> {
    size: Size<usize>,
    pages: Vec<Option<AtlasPage<T>>>,
    current_page: usize,
    cached_glyph_image_rects: HashMap<GlyphImageKey, (usize, Rect<usize>)>,
}

#[derive(Clone, Debug)]
struct AtlasPage<T> {
    image: Image<T>,
    dirty_rect: Rect<usize>,
    current_point: Point<usize>,
    current_row_height: usize,
}

impl<T: Clone + Default> AtlasPage<T> {
    fn new(size: Size<usize>) -> Self {
        Self {
            image: Image::new(size),
            dirty_rect: Rect::ZERO,
            current_point: Point::ZERO,
            current_row_height: 0,
        }
    }

    fn allocate(&mut self, size: Size<usize>) -> Option<Rect<usize>> {
        let mut origin = self.current_point;
        let mut row_height = self.current_row_height;
        if origin.x + size.width > self.image.size().width {
            origin.x = 0;
            origin.y += row_height;
            row_height = 0;
        }
        if origin.y + size.height > self.image.size().height {
            return None;
        }
        self.current_point = Point::new(origin.x + size.width, origin.y);
        self.current_row_height = row_height.max(size.height);
        let rect = Rect::new(origin, size);
        self.dirty_rect = self.dirty_rect.union(rect);
        Some(rect)
    }
}

impl<T: Clone + Default> FontAtlas<T> {
    pub fn new(size: Size<usize>) -> Self {
        Self {
            size,
            pages: vec![Some(AtlasPage::new(size))],
            current_page: 0,
            cached_glyph_image_rects: HashMap::new(),
        }
    }

    pub fn size(&self) -> Size<usize> {
        self.size
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn has_page(&self, page: usize) -> bool {
        self.pages.get(page).is_some_and(Option::is_some)
    }

    pub fn image(&self, page: usize) -> &Image<T> {
        &self.pages[page].as_ref().unwrap().image
    }

    pub unsafe fn replace_pixels(&mut self, page: usize, pixels: Vec<T>) -> Vec<T> {
        self.pages[page].as_mut().unwrap().image.replace_pixels(pixels)
    }

    pub fn take_dirty_image(&mut self, page: usize) -> Subimage<'_, T> {
        let page = self.pages[page].as_mut().unwrap();
        let dirty_rect = std::mem::replace(&mut page.dirty_rect, Rect::ZERO);
        page.image.subimage(dirty_rect)
    }

    /// The caller must first prove no retained draw call references this page.
    pub fn release_page(&mut self, page: usize) {
        assert_ne!(page, 0, "the primary font texture stays available");
        self.pages[page] = None;
        self.cached_glyph_image_rects.retain(|_, (id, _)| *id != page);
        if self.current_page == page {
            self.current_page = 0;
        }
    }

    pub fn get_or_allocate_glyph_image(&mut self, key: GlyphImageKey) -> Option<(usize, GlyphImage<'_, T>)> {
        if let Some(&(page, rect)) = self.cached_glyph_image_rects.get(&key) {
            return Some((page, GlyphImage::Cached(rect)));
        }
        // An oversized glyph cannot fit on any page; do not allocate pages forever.
        if key.size.width > self.size.width || key.size.height > self.size.height {
            return None;
        }
        let mut page = self.current_page;
        let rect = if let Some(rect) = self.pages[page].as_mut().unwrap().allocate(key.size) {
            rect
        } else {
            page = self.pages.iter().position(Option::is_none).unwrap_or(self.pages.len());
            let fresh = Some(AtlasPage::new(self.size));
            if page == self.pages.len() {
                self.pages.push(fresh);
            } else {
                self.pages[page] = fresh;
            }
            self.current_page = page;
            self.pages[page].as_mut().unwrap().allocate(key.size).unwrap()
        };
        self.cached_glyph_image_rects.insert(key, (page, rect));
        Some((page, GlyphImage::Allocated(self.pages[page].as_mut().unwrap().image.subimage_mut(rect))))
    }
}

pub type GrayscaleAtlas = FontAtlas<R>;

impl GrayscaleAtlas {
    pub fn save_to_png(&self, page: usize, path: impl AsRef<Path>) {
        let file = File::create(path).unwrap();
        let writer = BufWriter::new(file);
        let size = self.size();
        let mut encoder = png::Encoder::new(writer, size.width as u32, size.height as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = self.image(page).as_pixels();
        let data = unsafe { slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len()) };
        writer.write_image_data(&data).unwrap();
    }
}

pub type ColorAtlas = FontAtlas<Bgra>;

impl ColorAtlas {
    pub fn save_to_png(&self, page: usize, path: impl AsRef<Path>) {
        let file = File::create(path).unwrap();
        let writer = BufWriter::new(file);
        let size = self.size();
        let mut encoder = png::Encoder::new(writer, size.width as u32, size.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = self.image(page).as_pixels();
        let data = unsafe { slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len() * 4) };
        writer.write_image_data(&data).unwrap();
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GlyphImageKey {
    pub font_id: FontId,
    pub glyph_id: GlyphId,
    pub size: Size<usize>,
}

#[derive(Debug)]
pub enum GlyphImage<'a, T> {
    Cached(Rect<usize>),
    Allocated(SubimageMut<'a, T>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: GlyphId, size: usize) -> GlyphImageKey {
        GlyphImageKey { font_id: FontId::from("atlas-page-test"), glyph_id: id, size: Size::new(size, size) }
    }

    #[test]
    fn atlas_overflow_preserves_existing_glyph_pixels_and_coordinates() {
        let mut atlas = GrayscaleAtlas::new(Size::new(4, 4));
        let (first, GlyphImage::Allocated(mut image)) = atlas.get_or_allocate_glyph_image(key(1, 4)).unwrap() else { panic!() };
        image[Point::ZERO] = R { bits: 173 };
        let (second, GlyphImage::Allocated(mut image)) = atlas.get_or_allocate_glyph_image(key(2, 4)).unwrap() else { panic!() };
        image[Point::ZERO] = R { bits: 91 };
        assert_ne!(first, second);
        let (cached, GlyphImage::Cached(rect)) = atlas.get_or_allocate_glyph_image(key(1, 4)).unwrap() else { panic!() };
        assert_eq!(cached, first);
        assert_eq!(rect.origin, Point::ZERO);
        assert_eq!(atlas.image(first)[rect.origin].bits, 173);
        assert_eq!(atlas.image(second)[Point::ZERO].bits, 91);
    }

    #[test]
    fn atlas_recycles_only_the_released_page_and_invalidates_its_keys() {
        let mut atlas = GrayscaleAtlas::new(Size::new(4, 4));
        atlas.get_or_allocate_glyph_image(key(1, 4)).unwrap();
        let (released, _) = atlas.get_or_allocate_glyph_image(key(2, 4)).unwrap();
        let (retained, _) = atlas.get_or_allocate_glyph_image(key(3, 4)).unwrap();
        atlas.release_page(released);
        let (reused, glyph) = atlas.get_or_allocate_glyph_image(key(2, 4)).unwrap();
        assert_eq!(reused, released);
        assert!(matches!(glyph, GlyphImage::Allocated(_)));
        let (cached, glyph) = atlas.get_or_allocate_glyph_image(key(3, 4)).unwrap();
        assert_eq!(cached, retained);
        assert!(matches!(glyph, GlyphImage::Cached(_)));
        assert_eq!(atlas.page_count(), 3);
    }

    #[test]
    fn oversized_glyph_does_not_allocate_or_displace_cached_glyphs() {
        let mut atlas = GrayscaleAtlas::new(Size::new(4, 4));
        atlas.get_or_allocate_glyph_image(key(1, 4)).unwrap();
        assert!(atlas.get_or_allocate_glyph_image(key(2, 5)).is_none());
        assert_eq!(atlas.page_count(), 1);
        assert!(matches!(atlas.get_or_allocate_glyph_image(key(1, 4)), Some((0, GlyphImage::Cached(_)))));
    }
}
