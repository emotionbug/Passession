use {
    super::{
        //builtins,
        font::{Font, FontId},
        font_face::FontFace,
        font_family::{FontFamily, FontFamilyId},
        rasterizer,
        rasterizer::Rasterizer,
        shaper,
        shaper::Shaper,
    },
    std::{
        cell::RefCell,
        collections::{HashMap, HashSet},
        rc::Rc,
        sync::Arc,
    },
};

#[derive(Clone, Debug)]
pub struct Loader {
    shaper: Rc<RefCell<Shaper>>,
    rasterizer: Rc<RefCell<rasterizer::Rasterizer>>,
    font_family_definitions: HashMap<FontFamilyId, FontFamilyDefinition>,
    font_definitions: HashMap<FontId, FontDefinition>,
    font_family_cache: HashMap<FontFamilyId, Rc<FontFamily>>,
    font_cache: HashMap<FontId, Rc<Font>>,
    /// Definitions whose bytes did not parse at their face index. They are
    /// skipped by every family that lists them instead of aborting the app.
    failed_fonts: HashSet<FontId>,
}

impl Loader {
    pub fn new(settings: Settings) -> Self {
        let loader = Self {
            shaper: Rc::new(RefCell::new(Shaper::new(settings.shaper))),
            rasterizer: Rc::new(RefCell::new(Rasterizer::new(settings.rasterizer))),
            font_family_definitions: HashMap::new(),
            font_definitions: HashMap::new(),
            font_family_cache: HashMap::new(),
            font_cache: HashMap::new(),
            failed_fonts: HashSet::new(),
        };
        //builtins::define(&mut loader);
        loader
    }

    pub fn rasterizer(&self) -> &Rc<RefCell<Rasterizer>> {
        &self.rasterizer
    }

    pub fn is_font_family_known(&self, id: FontFamilyId) -> bool {
        if self.font_family_definitions.contains_key(&id) {
            return true;
        }
        if self.font_family_cache.contains_key(&id) {
            return true;
        }
        false
    }

    pub fn is_font_known(&self, id: FontId) -> bool {
        if self.font_definitions.contains_key(&id) {
            return true;
        }
        if self.font_cache.contains_key(&id) {
            return true;
        }
        self.failed_fonts.contains(&id)
    }

    pub fn define_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        debug_assert!(
            !self.is_font_family_known(id),
            "can't redefine a font family that is already known"
        );
        self.font_family_definitions.insert(id, definition);
    }

    /// Replaces the font list of a family, known or not. The loaded family is
    /// dropped so the next lookup rebuilds it from the new list; fonts stay
    /// cached by id, so faces shared with the old list are not reparsed.
    /// Layouts that already hold the old family keep it until they are dropped.
    pub fn redefine_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        self.font_family_cache.remove(&id);
        self.font_family_definitions.insert(id, definition);
    }

    pub fn define_font(&mut self, id: FontId, definition: FontDefinition) {
        debug_assert!(
            !self.is_font_known(id),
            "can't redefine a font that is already known"
        );
        self.font_definitions.insert(id, definition);
    }

    pub fn get_or_load_font_family(&mut self, id: FontFamilyId) -> &Rc<FontFamily> {
        if !self.font_family_cache.contains_key(&id) {
            let font_family = self.load_font_family(id);
            self.font_family_cache.insert(id, Rc::new(font_family));
        }
        self.font_family_cache.get(&id).unwrap()
    }

    fn load_font_family(&mut self, id: FontFamilyId) -> FontFamily {
        let definition = self
            .font_family_definitions
            .remove(&id)
            .unwrap_or_else(|| panic!("font family {:?} is not defined", id));
        FontFamily::new(
            id,
            self.shaper.clone(),
            definition
                .font_ids
                .into_iter()
                .filter_map(|font_id| self.get_or_load_font(font_id).cloned())
                .collect(),
        )
    }

    /// `None` when the font is not defined, or when its bytes do not parse at
    /// its face index (logged once and remembered).
    pub fn get_or_load_font(&mut self, id: FontId) -> Option<&Rc<Font>> {
        if !self.font_cache.contains_key(&id) {
            if self.failed_fonts.contains(&id) || !self.font_definitions.contains_key(&id) {
                return None;
            }
            match self.load_font(id) {
                Some(font) => {
                    self.font_cache.insert(id, Rc::new(font));
                }
                None => {
                    self.failed_fonts.insert(id);
                    return None;
                }
            }
        }
        self.font_cache.get(&id)
    }

    fn load_font(&mut self, id: FontId) -> Option<Font> {
        let definition = self.font_definitions.remove(&id)?;
        let Some(face) = FontFace::from_data_index_and_variations(
            definition.data,
            definition.index,
            &definition.variations,
        ) else {
            makepad_platform::error!(
                "skipping font {:?}: face {} does not parse",
                id,
                definition.index
            );
            return None;
        };
        Some(Font::new(
            id.clone(),
            self.rasterizer.clone(),
            face,
            definition.ascender_fudge_in_ems,
            definition.descender_fudge_in_ems,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub shaper: shaper::Settings,
    pub rasterizer: rasterizer::Settings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFamilyDefinition {
    pub font_ids: Vec<FontId>,
}

#[derive(Clone, Debug)]
pub struct FontDefinition {
    pub data: Arc<Vec<u8>>,
    /// Face index inside a collection (`.ttc`/`.otc`); 0 for a single font.
    pub index: u32,
    /// Variable-font instance as user-space axis coordinates, e.g.
    /// `(*b"wght", 600.0)`. Empty selects the default instance.
    pub variations: Vec<([u8; 4], f32)>,
    pub ascender_fudge_in_ems: f32,
    pub descender_fudge_in_ems: f32,
}
