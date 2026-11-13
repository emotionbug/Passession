use {
    super::{
        async_layout::{FontCatalog, Service},
        font::FontId,
        font_family::FontFamilyId,
        layouter::{self, LaidoutText, LayoutParams, Layouter},
        loader::{FontDefinition, FontFamilyDefinition},
        rasterizer::Rasterizer,
    },
    makepad_platform::*,
    std::{cell::RefCell, mem, rc::Rc, sync::Arc},
};

#[derive(Debug)]
pub struct Fonts {
    layouter: Layouter,
    pub(super) catalog: Arc<FontCatalog>,
    pub(super) async_service: Option<Arc<Service>>,
    pub(super) settings: layouter::Settings,
    needs_prepare_atlases: bool,
    grayscale_textures: Vec<Option<Texture>>,
    color_textures: Vec<Option<Texture>>,
}

impl Fonts {
    pub fn new(cx: &mut Cx, settings: layouter::Settings) -> Self {
        super::async_layout::enable_deferred_layout_drop();
        let layouter = Layouter::new(settings);
        let rasterizer = layouter.rasterizer().borrow();
        let grayscale_atlas_size = rasterizer.grayscale_atlas().size();
        let color_atlas_size = rasterizer.color_atlas().size();
        drop(rasterizer);
        Self {
            layouter,
            catalog: Arc::new(FontCatalog::default()),
            async_service: None,
            settings,
            needs_prepare_atlases: false,
            grayscale_textures: vec![Some(Texture::new_with_format(
                cx,
                TextureFormat::VecRu8 {
                    width: grayscale_atlas_size.width,
                    height: grayscale_atlas_size.height,
                    data: None,
                    unpack_row_length: None,
                    updated: TextureUpdated::Empty,
                },
            ))],
            color_textures: vec![Some(Texture::new_with_format(
                cx,
                TextureFormat::VecBGRAu8_32 {
                    width: color_atlas_size.width,
                    height: color_atlas_size.height,
                    data: None,
                    updated: TextureUpdated::Empty,
                },
            ))],
        }
    }

    pub fn rasterizer(&self) -> &Rc<RefCell<Rasterizer>> {
        self.layouter.rasterizer()
    }

    pub fn grayscale_texture(&mut self, cx: &mut Cx, page: usize) -> &Texture {
        self.grayscale_textures.resize_with(self.grayscale_textures.len().max(page + 1), || None);
        if self.grayscale_textures[page].is_none() {
            let size = self.rasterizer().borrow().grayscale_atlas().size();
            self.grayscale_textures[page] = Some(Texture::new_with_format(cx, TextureFormat::VecRu8 {
                width: size.width, height: size.height, data: None,
                unpack_row_length: None, updated: TextureUpdated::Empty,
            }));
        }
        self.grayscale_textures[page].as_ref().unwrap()
    }

    pub fn color_texture(&mut self, cx: &mut Cx, page: usize) -> &Texture {
        self.color_textures.resize_with(self.color_textures.len().max(page + 1), || None);
        if self.color_textures[page].is_none() {
            let size = self.rasterizer().borrow().color_atlas().size();
            self.color_textures[page] = Some(Texture::new_with_format(cx, TextureFormat::VecBGRAu8_32 {
                width: size.width, height: size.height, data: None, updated: TextureUpdated::Empty,
            }));
        }
        self.color_textures[page].as_ref().unwrap()
    }

    pub fn is_font_family_known(&self, id: FontFamilyId) -> bool {
        self.layouter.is_font_family_known(id)
    }

    pub fn is_font_known(&self, id: FontId) -> bool {
        self.layouter.is_font_known(id)
    }

    pub fn define_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        let catalog = Arc::make_mut(&mut self.catalog);
        catalog.revision += 1;
        catalog.families.insert(id, definition.clone());
        self.layouter.define_font_family(id, definition);
    }

    /// Replaces the font list of an existing (or new) family at runtime. The
    /// UI layouter drops the family and its layout cache; the catalog revision
    /// advances so asynchronous layouts requested against the old list are not
    /// published and the worker rebuilds the family for the next request.
    /// Callers redraw afterwards; retained `Rc<LaidoutText>` values outside
    /// the layouter keep the old fonts until they are laid out again.
    pub fn redefine_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        let catalog = Arc::make_mut(&mut self.catalog);
        catalog.revision += 1;
        catalog.families.insert(id, definition.clone());
        self.layouter.redefine_font_family(id, definition);
    }

    /// The current font list of a family, as last defined or redefined.
    pub fn font_family_definition(&self, id: FontFamilyId) -> Option<&FontFamilyDefinition> {
        self.catalog.families.get(&id)
    }

    /// Loads a defined font on the UI layouter (e.g. to read its metrics).
    /// `None` when the font is undefined or its face does not parse.
    pub fn get_or_load_font(&mut self, id: FontId) -> Option<Rc<super::font::Font>> {
        self.layouter.try_load_font(id)
    }

    pub fn define_font(&mut self, id: FontId, definition: FontDefinition) {
        let catalog = Arc::make_mut(&mut self.catalog);
        catalog.revision += 1;
        catalog.fonts.insert(id, definition.clone());
        self.layouter.define_font(id, definition);
    }

    pub fn get_or_layout(&mut self, params: impl LayoutParams) -> Rc<LaidoutText> {
        self.layouter.get_or_layout(params)
    }

    pub(super) fn load_font(&mut self, id: FontId) -> Rc<super::font::Font> {
        self.layouter.load_font(id)
    }

    pub fn prepare_textures(&mut self, cx: &mut Cx) {
        assert!(!self.needs_prepare_atlases);
        let grayscale_pages = self.rasterizer().borrow().grayscale_atlas().page_count();
        for page in 0..grayscale_pages {
            if !self.rasterizer().borrow().grayscale_atlas().has_page(page) {
                continue;
            }
            self.grayscale_texture(cx, page);
            let mut rasterizer = self.layouter.rasterizer().borrow_mut();
            let atlas = rasterizer.grayscale_atlas_mut();
            let dirty_rect = atlas.take_dirty_image(page).bounds();
            let pixels: Vec<u8> = unsafe { mem::transmute(atlas.replace_pixels(page, Vec::new())) };
            self.grayscale_textures[page].as_ref().unwrap().put_back_vec_u8(cx, pixels, Some(RectUsize::new(
                PointUsize::new(dirty_rect.origin.x, dirty_rect.origin.y),
                SizeUsize::new(dirty_rect.size.width, dirty_rect.size.height),
            )));
        }
        let color_pages = self.rasterizer().borrow().color_atlas().page_count();
        for page in 0..color_pages {
            if !self.rasterizer().borrow().color_atlas().has_page(page) {
                continue;
            }
            self.color_texture(cx, page);
            let mut rasterizer = self.layouter.rasterizer().borrow_mut();
            let atlas = rasterizer.color_atlas_mut();
            let dirty_rect = atlas.take_dirty_image(page).bounds();
            let pixels: Vec<u32> = unsafe { mem::transmute(atlas.replace_pixels(page, Vec::new())) };
            self.color_textures[page].as_ref().unwrap().put_back_vec_u32(cx, pixels, Some(RectUsize::new(
                PointUsize::new(dirty_rect.origin.x, dirty_rect.origin.y),
                SizeUsize::new(dirty_rect.size.width, dirty_rect.size.height),
            )));
        }
        self.needs_prepare_atlases = true;
    }

    pub fn prepare_atlases_if_needed(&mut self, cx: &mut Cx) {
        if !self.needs_prepare_atlases {
            return;
        }
        let mut rasterizer = self.layouter.rasterizer().borrow_mut();
        for (page, slot) in self.grayscale_textures.iter_mut().enumerate() {
            let Some(texture) = slot.as_ref() else { continue };
            let pixels = texture.take_vec_u8(cx);
            if page != 0 && texture.is_unique() {
                // A cached draw list keeps a Texture clone, so its UVs can never
                // point into a recycled page. Only unreferenced pages are retired.
                rasterizer.grayscale_atlas_mut().release_page(page);
                *slot = None;
            } else {
                unsafe { rasterizer.grayscale_atlas_mut().replace_pixels(page, mem::transmute(pixels)); }
            }
        }
        for (page, slot) in self.color_textures.iter_mut().enumerate() {
            let Some(texture) = slot.as_ref() else { continue };
            let pixels = texture.take_vec_u32(cx);
            if page != 0 && texture.is_unique() {
                rasterizer.color_atlas_mut().release_page(page);
                *slot = None;
            } else {
                unsafe { rasterizer.color_atlas_mut().replace_pixels(page, mem::transmute(pixels)); }
            }
        }
        self.needs_prepare_atlases = false;
    }
}
