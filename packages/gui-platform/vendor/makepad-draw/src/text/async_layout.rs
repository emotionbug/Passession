//! Exact CPU layout with thread-local fonts and bounded UI glyph hydration.
//!
//! `request` only clones shared source/definition handles. The caller assigns a
//! revision to each source/span snapshot; requests never hash document content.
//! Font registration (including converting dependency bytes to Arc) and loading
//! a previously unseen font face remain indivisible operations. Row/glyph buffer
//! reservation is also an allocator operation, not a claimed wall-clock bound.
use {
    super::{
        color::Color,
        font::{Font, FontId, GlyphId},
        font_family::FontFamilyId,
        fonts::Fonts,
        geom::{Point, Rect, Size},
        layouter::{LaidoutGlyph, LaidoutRow, LaidoutText, LayoutOptions, Layouter, Settings, Span},
        loader::{FontDefinition, FontFamilyDefinition},
        source_index::{GlyphIndex, SourceIndex},
        substr::Substr,
    },
    makepad_platform::SignalToUI,
    std::{
        cell::{Cell, RefCell},
        collections::{HashMap, VecDeque},
        rc::Rc,
        sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Condvar, Mutex},
    },
};

thread_local! {
    static DEFER_LAYOUT_DROP: Cell<bool> = const { Cell::new(false) };
    static RETIRED_ROWS: RefCell<VecDeque<Vec<LaidoutRow>>> = const { RefCell::new(VecDeque::new()) };
}

pub(super) fn enable_deferred_layout_drop() {
    DEFER_LAYOUT_DROP.with(|enabled| enabled.set(true));
}

pub(super) fn retire_rows(rows: &mut Vec<LaidoutRow>) {
    if rows.is_empty() { return; }
    // Workers have no Fonts/UI and destroy their own local Rc values directly.
    if DEFER_LAYOUT_DROP.try_with(Cell::get).unwrap_or(false) {
        let _ = RETIRED_ROWS.try_with(|retired| retired.borrow_mut().push_back(std::mem::take(rows)));
        SignalToUI::set_ui_signal();
    }
}

/// Drain deferred layout destruction on the owning UI thread without drawing.
/// Returns true when more work remains after at most 4096 glyphs and 128 rows.
/// The caller must schedule another paced drain when true; this function never
/// emits a UI signal. Retiring the first snapshot supplies the initial wakeup.
pub fn collect_retired_layouts() -> bool {
    let mut glyph_budget = 4096;
    let mut row_budget = 128;
    RETIRED_ROWS.with(|retired| {
        let mut retired = retired.borrow_mut();
        while let Some(rows) = retired.front_mut() {
            while let Some(row) = rows.last_mut() {
                if row_budget == 0 { return true; }
                while !row.glyphs.is_empty() && glyph_budget > 0 { row.glyphs.pop(); glyph_budget -= 1; }
                if !row.glyphs.is_empty() { return true; }
                rows.pop();
                row_budget -= 1;
            }
            retired.pop_front();
        }
        false
    })
}

#[derive(Clone, Debug, Default)]
pub(super) struct FontCatalog {
    pub revision: u64,
    pub fonts: HashMap<FontId, FontDefinition>,
    pub families: HashMap<FontFamilyId, FontFamilyDefinition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Key {
    revision: u64,
    fonts: u64,
    options: LayoutOptions,
}

#[derive(Debug)]
struct TransferGlyph {
    origin: Point<f32>,
    font: FontId,
    size: f32,
    color: Option<Color>,
    id: GlyphId,
    cluster: usize,
    advance: f32,
    offset: f32,
    bounds: Rect<f32>,
}

#[derive(Debug)]
struct TransferRow {
    origin: Point<f32>,
    start: usize,
    end: usize,
    newline: bool,
    width: f32,
    ascender: f32,
    descender: f32,
    line_gap: f32,
    above: f32,
    below: f32,
    glyphs: Vec<TransferGlyph>,
    glyph_index: Arc<GlyphIndex>,
}

#[derive(Debug)]
struct Transfer {
    key: Key,
    text: Arc<str>,
    size: Size<f32>,
    rows: Vec<TransferRow>,
    source_index: Arc<SourceIndex>,
}

impl Transfer {
    // Executed only by the worker. No Font, Rc, rasterizer, or texture escapes.
    fn from_layout(key: Key, mut text: LaidoutText) -> Self {
        Self {
            key,
            text: text.text.parent().clone(),
            size: text.size_in_lpxs,
            source_index: text.source_index.clone(),
            rows: std::mem::take(&mut text.rows).into_iter().map(|row| TransferRow {
                origin: row.origin_in_lpxs,
                start: row.text.start_in_parent(),
                end: row.text.end_in_parent(),
                newline: row.newline,
                width: row.width_in_lpxs,
                ascender: row.ascender_in_lpxs,
                descender: row.descender_in_lpxs,
                line_gap: row.line_gap_in_lpxs,
                above: row.line_spacing_above_in_lpxs,
                below: row.line_spacing_below_in_lpxs,
                glyph_index: row.glyph_index,
                glyphs: row.glyphs.into_iter().map(|glyph| TransferGlyph {
                    origin: glyph.origin_in_lpxs,
                    font: glyph.font.id(),
                    size: glyph.font_size_in_lpxs,
                    color: glyph.color,
                    id: glyph.id,
                    cluster: glyph.cluster,
                    advance: glyph.advance_in_ems,
                    offset: glyph.offset_in_ems,
                    bounds: glyph.paint_bounds_in_ems,
                }).collect(),
            }).collect(),
        }
    }
}

#[derive(Debug, Default)]
struct Mailbox {
    cancelled: AtomicBool,
    result: Mutex<Option<Transfer>>,
}

#[derive(Debug)]
struct Job {
    owner: u64,
    key: Key,
    text: Arc<str>,
    spans: Arc<Vec<Span>>,
    catalog: Arc<FontCatalog>,
    mailbox: Arc<Mailbox>,
}

const MAX_QUEUED_LAYOUTS: usize = 64;

#[derive(Debug, Default)]
struct Queue {
    jobs: VecDeque<Job>,
    garbage: Vec<Transfer>,
    stopped: bool,
}

/// Brings a worker layouter's families in line with `catalog`: new families
/// are defined, families whose font list changed are redefined (dropping the
/// stale loaded family), unchanged ones are left loaded.
fn apply_catalog_families(
    layouter: &mut Layouter,
    applied: &mut HashMap<FontFamilyId, FontFamilyDefinition>,
    catalog: &FontCatalog,
) {
    for (&id, definition) in &catalog.families {
        match applied.get(&id) {
            Some(current) if current == definition => continue,
            Some(_) => layouter.redefine_font_family(id, definition.clone()),
            None => layouter.define_font_family(id, definition.clone()),
        }
        applied.insert(id, definition.clone());
    }
}

#[derive(Debug)]
pub(super) struct Service {
    state: Arc<(Mutex<Queue>, Condvar)>,
}

impl Service {
    fn new(mut settings: Settings) -> Arc<Self> {
        let state = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker_state = state.clone();
        // CPU layout never rasterizes. Avoid allocating unused UI-sized atlases.
        settings.loader.rasterizer.grayscale_atlas_size = Size::new(1, 1);
        settings.loader.rasterizer.color_atlas_size = Size::new(1, 1);
        std::thread::Builder::new().name("makepad-exact-layout".into()).spawn(move || {
            let mut layouter = Layouter::new(settings);
            // The family lists this worker's layouter was given, to notice
            // `Fonts::redefine_font_family` in a newer catalog.
            let mut applied_families: HashMap<FontFamilyId, FontFamilyDefinition> = HashMap::new();
            loop {
                let (job, garbage) = {
                    let (lock, wake) = &*worker_state;
                    let mut queue = lock.lock().unwrap();
                    while queue.jobs.is_empty() && queue.garbage.is_empty() && !queue.stopped {
                        queue = wake.wait(queue).unwrap();
                    }
                    if queue.stopped { return; }
                    (queue.jobs.pop_front(), std::mem::take(&mut queue.garbage))
                };
                drop(garbage);
                let Some(job) = job else { continue };
                // Admission capacity just opened, even if this job takes a long
                // time or belongs to an owner that has since disappeared.
                SignalToUI::set_ui_signal();
                if job.mailbox.cancelled.load(Ordering::Relaxed) { continue; }
                for (&id, definition) in &job.catalog.fonts {
                    if !layouter.is_font_known(id) { layouter.define_font(id, definition.clone()); }
                }
                apply_catalog_families(&mut layouter, &mut applied_families, &job.catalog);
                // Finish admitted work even when producers continuously update.
                // At most one latest waiting request exists for each owner.
                let text = layouter.layout_shared(job.text.into(), &job.spans, job.key.options);
                let result = Transfer::from_layout(job.key, text);
                if !job.mailbox.cancelled.load(Ordering::Relaxed) {
                    let old = job.mailbox.result.lock().unwrap().replace(result);
                    drop(old);
                    SignalToUI::set_ui_signal();
                }
            }
        }).expect("failed to start exact text layout worker");
        Arc::new(Self { state })
    }

    fn submit(&self, job: Job) -> Result<(), Job> {
        let (lock, wake) = &*self.state;
        let mut queue = lock.lock().unwrap();
        queue.jobs.retain(|waiting| !waiting.mailbox.cancelled.load(Ordering::Relaxed));
        if let Some(waiting) = queue.jobs.iter_mut().find(|waiting| waiting.owner == job.owner) {
            *waiting = job;
        } else {
            if queue.jobs.len() >= MAX_QUEUED_LAYOUTS { return Err(job); }
            queue.jobs.push_back(job);
        }
        wake.notify_one();
        Ok(())
    }

    fn retire(&self, transfer: Transfer) {
        let (lock, wake) = &*self.state;
        lock.lock().unwrap().garbage.push(transfer);
        wake.notify_one();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let (lock, wake) = &*self.state;
        lock.lock().unwrap().stopped = true;
        wake.notify_one();
    }
}

#[derive(Debug)]
struct Hydration {
    transfer: Transfer,
    text: LaidoutText,
    row: Option<LaidoutRow>,
}

impl Hydration {
    fn new(transfer: Transfer) -> Self {
        Self {
            text: LaidoutText {
                text: Substr::from(transfer.text.clone()),
                size_in_lpxs: transfer.size,
                // Reserve once: incremental pushes never copy all prior rows.
                rows: Vec::with_capacity(transfer.rows.len()),
                source_index: transfer.source_index.clone(),
            },
            transfer,
            row: None,
        }
    }

    fn advance(&mut self, mut load_font: impl FnMut(FontId) -> Rc<Font>, glyph_budget: &mut usize, row_budget: &mut usize) -> bool {
        while self.text.rows.len() < self.transfer.rows.len() && *row_budget > 0 {
            let source = &self.transfer.rows[self.text.rows.len()];
            let row = self.row.get_or_insert_with(|| LaidoutRow {
                origin_in_lpxs: source.origin,
                text: self.text.text.substr(source.start..source.end),
                newline: source.newline,
                width_in_lpxs: source.width,
                ascender_in_lpxs: source.ascender,
                descender_in_lpxs: source.descender,
                line_gap_in_lpxs: source.line_gap,
                line_spacing_above_in_lpxs: source.above,
                line_spacing_below_in_lpxs: source.below,
                // Includes the unwrapped giant-row case: no growing Vec copies.
                glyphs: Vec::with_capacity(source.glyphs.len()),
                glyph_index: source.glyph_index.clone(),
            });
            let end = row.glyphs.len().saturating_add(*glyph_budget).min(source.glyphs.len());
            for glyph in &source.glyphs[row.glyphs.len()..end] {
                row.glyphs.push(LaidoutGlyph {
                    origin_in_lpxs: glyph.origin,
                    font: load_font(glyph.font),
                    font_size_in_lpxs: glyph.size,
                    color: glyph.color,
                    id: glyph.id,
                    cluster: glyph.cluster,
                    advance_in_ems: glyph.advance,
                    offset_in_ems: glyph.offset,
                    paint_bounds_in_ems: glyph.bounds,
                });
                *glyph_budget -= 1;
            }
            if row.glyphs.len() != source.glyphs.len() { return false; }
            self.text.rows.push(self.row.take().unwrap());
            *row_budget -= 1;
        }
        self.text.rows.len() == self.transfer.rows.len()
    }
}

/// One caller-owned layout slot. Keep it alive across draws. A revision names an
/// immutable text/span snapshot; changing either requires a different revision.
/// `completed` remains coherent while a newer request is pending. A completion
/// for outdated source, layout options, or font catalog is never published.
#[derive(Debug)]
pub struct AsyncLayout {
    owner: u64,
    service: Option<Arc<Service>>,
    mailbox: Arc<Mailbox>,
    requested: Option<Key>,
    pending_admission: Option<Job>,
    completed_key: Option<Key>,
    completed: Option<Rc<LaidoutText>>,
    hydration: Option<Hydration>,
}

impl Default for AsyncLayout {
    fn default() -> Self {
        static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            service: None,
            mailbox: Arc::new(Mailbox::default()),
            requested: None,
            pending_admission: None,
            completed_key: None,
            completed: None,
            hydration: None,
        }
    }
}

impl AsyncLayout {
    pub fn new() -> Self { Self::default() }

    pub fn request(&mut self, fonts: &mut Fonts, revision: u64, text: Arc<str>, spans: Arc<Vec<Span>>, options: LayoutOptions) {
        let settings = fonts.settings;
        let service = fonts.async_service.get_or_insert_with(|| Service::new(settings));
        if let Some(previous) = &self.service {
            assert!(Arc::ptr_eq(previous, service), "AsyncLayout must stay with its Fonts instance");
        } else {
            self.service = Some(service.clone());
        }
        let key = Key { revision, fonts: fonts.catalog.revision, options };
        if self.requested != Some(key) {
            self.requested = Some(key);
            self.pending_admission = Some(Job {
                owner: self.owner, key, text, spans,
                catalog: fonts.catalog.clone(), mailbox: self.mailbox.clone(),
            });
        }
        self.retry_admission();
    }

    fn retry_admission(&mut self) {
        if let Some(job) = self.pending_admission.take() {
            // A full queue returns ownership of every source/span/catalog Arc.
            // The live caller retains its latest demand and retries without
            // rebuilding or hashing any document data.
            self.pending_admission = self.service.as_ref().unwrap().submit(job).err();
        }
    }

    /// Convert no more than these glyph/row counts on the UI thread. Returns true
    /// only when a fully hydrated, exactly current snapshot replaces `completed`.
    /// Call during measurement/event processing, not again for painting.
    pub fn poll(&mut self, fonts: &mut Fonts, max_glyphs: usize, max_rows: usize) -> bool {
        self.retry_admission();
        let mut glyphs = max_glyphs;
        let mut rows = max_rows;
        if self.hydration.is_none() {
            let result = self.mailbox.result.lock().unwrap().take();
            if let Some(result) = result {
                self.hydration = Some(Hydration::new(result));
            }
        }
        let Some(hydration) = &mut self.hydration else { return false };
        if !hydration.advance(|id| fonts.load_font(id), &mut glyphs, &mut rows) {
            SignalToUI::set_ui_signal();
            return false;
        }
        let hydration = self.hydration.take().unwrap();
        let key = hydration.transfer.key;
        self.service.as_ref().unwrap().retire(hydration.transfer);
        let current = self.requested == Some(key) && key.fonts == fonts.catalog.revision;
        if current {
            self.completed = Some(Rc::new(hydration.text));
            self.completed_key = Some(key);
        } else {
            drop(hydration.text);
        }
        if self.is_pending() { SignalToUI::set_ui_signal(); }
        current
    }

    pub fn completed(&self) -> Option<&Rc<LaidoutText>> { self.completed.as_ref() }

    pub fn is_pending(&self) -> bool { self.requested != self.completed_key }
}

impl Drop for AsyncLayout {
    fn drop(&mut self) {
        self.mailbox.cancelled.store(true, Ordering::Relaxed);
        if let Some(service) = &self.service {
            let (lock, _) = &*service.state;
            let opened_slot = {
                let mut queue = lock.lock().unwrap();
                let previous_len = queue.jobs.len();
                queue.jobs.retain(|job| job.owner != self.owner);
                queue.jobs.len() != previous_len
            };
            if opened_slot { SignalToUI::set_ui_signal(); }
            if let Some(mut hydration) = self.hydration.take() {
                if let Some(row) = hydration.row.take() { hydration.text.rows.push(row); }
                service.retire(hydration.transfer);
            }
            if let Some(result) = self.mailbox.result.lock().unwrap().take() { service.retire(result); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{layouter::Style, selection::{Cursor, Selection}};

    fn fixture() -> (Layouter, Style) {
        let mut settings = Settings::default();
        settings.loader.rasterizer.grayscale_atlas_size = Size::new(1, 1);
        settings.loader.rasterizer.color_atlas_size = Size::new(1, 1);
        let mut layouter = Layouter::new(settings);
        let id = FontId::from(901);
        let family = FontFamilyId::from(902);
        layouter.define_font(id, FontDefinition {
            data: Arc::new(include_bytes!("../../../makepad-widgets/resources/LiberationMono-Regular.ttf").to_vec()),
            index: 0,
            variations: Vec::new(),
            ascender_fudge_in_ems: 0.125,
            descender_fudge_in_ems: -0.0625,
        });
        let fallback = FontId::from(903);
        layouter.define_font(fallback, FontDefinition {
            data: Arc::new(include_bytes!("../../../makepad-widgets/resources/fa-solid-900.ttf").to_vec()),
            index: 0,
            variations: Vec::new(),
            ascender_fudge_in_ems: 0.0,
            descender_fudge_in_ems: 0.0,
        });
        layouter.define_font_family(family, FontFamilyDefinition { font_ids: vec![id, fallback] });
        (layouter, Style { font_family_id: family, font_size_in_pts: 12.0, color: Some(Color::RED) })
    }

    fn compare(expected: &LaidoutText, actual: &LaidoutText) {
        assert!(expected.text.shallow_eq(&actual.text));
        assert_eq!(expected.size_in_lpxs, actual.size_in_lpxs);
        assert_eq!(expected.rows.len(), actual.rows.len());
        for (a, b) in expected.rows.iter().zip(&actual.rows) {
            assert!(a.text.shallow_eq(&b.text));
            assert_eq!(a.origin_in_lpxs, b.origin_in_lpxs);
            assert_eq!(a.newline, b.newline);
            assert_eq!([a.width_in_lpxs, a.ascender_in_lpxs, a.descender_in_lpxs, a.line_gap_in_lpxs, a.line_spacing_above_in_lpxs, a.line_spacing_below_in_lpxs],
                [b.width_in_lpxs, b.ascender_in_lpxs, b.descender_in_lpxs, b.line_gap_in_lpxs, b.line_spacing_above_in_lpxs, b.line_spacing_below_in_lpxs]);
            assert_eq!(a.glyphs.len(), b.glyphs.len());
            for (a, b) in a.glyphs.iter().zip(&b.glyphs) {
                assert_eq!(a.origin_in_lpxs, b.origin_in_lpxs);
                assert_eq!(a.font.id(), b.font.id());
                assert_eq!(a.id, b.id);
                assert_eq!(a.cluster, b.cluster);
                assert_eq!(a.color, b.color);
                assert_eq!([a.font_size_in_lpxs, a.advance_in_ems, a.offset_in_ems], [b.font_size_in_lpxs, b.advance_in_ems, b.offset_in_ems]);
                assert_eq!(a.paint_bounds_in_lpxs(), b.paint_bounds_in_lpxs());
            }
        }
        for (index, _) in expected.text.char_indices() {
            let cursor = Cursor { index, prefer_next_row: true };
            let a = expected.cursor_to_position(cursor);
            let b = actual.cursor_to_position(cursor);
            assert_eq!((a.row_index, a.x_in_lpxs), (b.row_index, b.x_in_lpxs));
            assert_eq!(expected.text_prefix_hash(index), actual.text_prefix_hash(index));
            assert_eq!(expected.word_at(index), actual.word_at(index));
        }
        let selection = Selection { anchor: Cursor { index: 0, prefer_next_row: true }, cursor: Cursor { index: expected.text.len(), prefer_next_row: false } };
        assert_eq!(expected.selection_rects_in_lpxs_clipped(selection, 30.0, 100.0), actual.selection_rects_in_lpxs_clipped(selection, 30.0, 100.0));
    }

    #[test]
    fn transferable_geometry_is_send_and_hydrates_exactly_with_tiny_budgets() {
        fn assert_send<T: Send>() {}
        assert_send::<Transfer>();
        assert_send::<Job>();
        let (mut worker, style) = fixture();
        let (mut ui, _) = fixture();
        for (source, wrap) in [
            ("a word.   café\n\ncombining e\u{301} 👩‍💻 日本語\u{a0}joined \u{f002}".repeat(80), true),
            ("unwrapped🙂".repeat(2048), false),
        ] {
            let text: Arc<str> = source.into();
            let spans = [
                Span { style, len: 3 },
                Span { style: Style { font_size_in_pts: 14.0, color: Some(Color::GREEN), ..style }, len: text.len() - 3 },
            ];
            let options = LayoutOptions { wrap, max_width_in_lpxs: Some(143.0), align: 0.25, line_spacing_scale: 1.25, first_row_indent_in_lpxs: 13.0, ..LayoutOptions::default() };
            let expected = worker.layout_shared(text.clone().into(), &spans, options);
            let background_text = text.clone();
            let background_spans = spans.clone();
            let transfer = std::thread::spawn(move || {
                let (mut worker, _) = fixture();
                let layout = worker.layout_shared(background_text.into(), &background_spans, options);
                Transfer::from_layout(Key { revision: 1, fonts: 1, options }, layout)
            }).join().unwrap();
            let mut hydration = Hydration::new(transfer);
            loop {
                let before: usize = hydration.text.rows.iter().map(|row| row.glyphs.len()).sum::<usize>() + hydration.row.as_ref().map_or(0, |row| row.glyphs.len());
                let before_rows = hydration.text.rows.len();
                let mut glyphs = 7;
                let mut rows = 2;
                let ready = hydration.advance(|id| ui.load_font(id), &mut glyphs, &mut rows);
                let after: usize = hydration.text.rows.iter().map(|row| row.glyphs.len()).sum::<usize>() + hydration.row.as_ref().map_or(0, |row| row.glyphs.len());
                assert!(after - before <= 7);
                assert!(hydration.text.rows.len() - before_rows <= 2);
                if ready { break; }
            }
            compare(&expected, &hydration.text);
        }
    }

    #[test]
    fn prefix_index_preserves_exact_bytes_across_samples() {
        let (mut layouter, style) = fixture();
        let text: Arc<str> = "aé🙂 ".repeat(1800).into();
        let result = layouter.layout_shared(text.clone().into(), &[Span { style, len: text.len() }], LayoutOptions::default());
        let mut hash = 0u64;
        for (index, byte) in text.bytes().enumerate() {
            assert_eq!(result.text_prefix_hash(index), hash);
            hash = hash.wrapping_mul(257).wrapping_add(byte as u64 + 1);
        }
        assert_eq!(result.text_prefix_hash(text.len()), hash);
    }

    #[test]
    fn queued_owners_keep_fifo_position_and_only_the_latest_revision() {
        let service = Service { state: Arc::new((Mutex::new(Queue::default()), Condvar::new())) };
        let job = |owner, revision| Job {
            owner,
            key: Key { revision, fonts: 0, options: LayoutOptions::default() },
            text: Arc::from("source"),
            spans: Arc::new(Vec::new()),
            catalog: Arc::new(FontCatalog::default()),
            mailbox: Arc::new(Mailbox::default()),
        };
        service.submit(job(1, 1)).unwrap();
        service.submit(job(2, 1)).unwrap();
        for revision in 2..100 { service.submit(job(1, revision)).unwrap(); }
        let queue = service.state.0.lock().unwrap();
        assert_eq!(queue.jobs.len(), 2);
        assert_eq!((queue.jobs[0].owner, queue.jobs[0].key.revision), (1, 99));
        assert_eq!((queue.jobs[1].owner, queue.jobs[1].key.revision), (2, 1));
    }

    #[test]
    fn full_queue_preserves_demand_and_retries_admission() {
        let service = Arc::new(Service { state: Arc::new((Mutex::new(Queue::default()), Condvar::new())) });
        let job = |owner| Job {
            owner,
            key: Key { revision: 7, fonts: 0, options: LayoutOptions::default() },
            text: Arc::from("retained source"),
            spans: Arc::new(Vec::new()),
            catalog: Arc::new(FontCatalog::default()),
            mailbox: Arc::new(Mailbox::default()),
        };
        for owner in 0..MAX_QUEUED_LAYOUTS as u64 { service.submit(job(owner)).unwrap(); }
        let mut layout = AsyncLayout::new();
        layout.owner = 1000;
        layout.service = Some(service.clone());
        let demand = job(layout.owner);
        layout.requested = Some(demand.key);
        let source = demand.text.clone();
        layout.pending_admission = Some(demand);
        layout.retry_admission();
        assert!(layout.is_pending());
        assert!(Arc::ptr_eq(&source, &layout.pending_admission.as_ref().unwrap().text));
        assert_eq!(service.state.0.lock().unwrap().jobs.len(), MAX_QUEUED_LAYOUTS);
        // Cancellation pruning opens a slot without dropping the retained demand.
        service.state.0.lock().unwrap().jobs[0].mailbox.cancelled.store(true, Ordering::Relaxed);
        layout.retry_admission();
        assert!(layout.pending_admission.is_none());
        let queue = service.state.0.lock().unwrap();
        assert_eq!(queue.jobs.len(), MAX_QUEUED_LAYOUTS);
        assert_eq!(queue.jobs.back().unwrap().owner, 1000);
        assert!(Arc::ptr_eq(&source, &queue.jobs.back().unwrap().text));
    }

    #[test]
    fn settings_participate_in_completion_identity() {
        let original = Key { revision: 1, fonts: 1, options: LayoutOptions::default() };
        let mut changed = original;
        changed.options.wrap = !changed.options.wrap;
        assert_ne!(original, changed);
        changed = original;
        changed.options.line_spacing_scale = 1.5;
        assert_ne!(original, changed);
        changed = original;
        changed.fonts += 1;
        assert_ne!(original, changed);
    }

    #[test]
    fn a_family_redefinition_withholds_the_stale_result_and_relayouts_with_new_fonts() {
        let mut cx = makepad_platform::Cx::new(Box::new(|_, _| {}));
        let mut fonts = Fonts::new(&mut cx, Settings::default());
        let definition = |data: &[u8]| FontDefinition {
            data: Arc::new(data.to_vec()),
            index: 0,
            variations: Vec::new(),
            ascender_fudge_in_ems: 0.0,
            descender_fudge_in_ems: 0.0,
        };
        let (mono, sans, family) = (FontId::from(921), FontId::from(922), FontFamilyId::from(923));
        fonts.define_font(mono, definition(include_bytes!("../../../makepad-widgets/resources/LiberationMono-Regular.ttf")));
        fonts.define_font(sans, definition(include_bytes!("../../../makepad-widgets/resources/NotoSans-Regular.ttf")));
        fonts.define_font_family(family, FontFamilyDefinition { font_ids: vec![mono] });
        let text: Arc<str> = "Illuminate".into();
        let spans = Arc::new(vec![Span { style: Style { font_family_id: family, font_size_in_pts: 12.0, color: None }, len: text.len() }]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

        let mut layout = AsyncLayout::new();
        layout.request(&mut fonts, 1, text.clone(), spans.clone(), LayoutOptions::default());
        while layout.mailbox.result.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "worker never produced a layout");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        fonts.redefine_font_family(family, FontFamilyDefinition { font_ids: vec![sans, mono] });
        assert!(!layout.poll(&mut fonts, usize::MAX, usize::MAX), "stale catalog revision was published");
        assert!(layout.completed().is_none());

        // The same source revision under the new catalog reaches the worker,
        // which must rebuild the family it already loaded.
        layout.request(&mut fonts, 1, text, spans, LayoutOptions::default());
        while !layout.poll(&mut fonts, usize::MAX, usize::MAX) {
            assert!(std::time::Instant::now() < deadline, "worker never relaid out the redefined family");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let completed = layout.completed().unwrap();
        let glyphs: Vec<_> = completed.rows.iter().flat_map(|row| &row.glyphs).collect();
        assert_eq!(glyphs.len(), "Illuminate".len());
        assert!(glyphs.iter().all(|glyph| glyph.font.id() == sans));
    }
}
