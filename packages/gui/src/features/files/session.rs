//! One file tab's file (Flutter `file_editor_session.dart`, original `FilePane`): observes the
//! file through a [`FileHandle`], resolves its panel from the stat, and mounts that panel's file
//! state — the editor buffer, the read-only text or the image bytes.
//!
//! Sans-IO like the handle: [`FileSession::pump`] routes handle events and job outcomes to the
//! mounted model/readers; the store executes the handle's commands.

use crate::features::files::handle::{
    FileHandle, FileVersion, HandleEvent, JobResult, Outcome, Purpose, Snapshot, WriteError,
};
use crate::features::files::model::{FileEditorModel, FileEvent, ModelIo, WriteFailure};
use crate::features::files::resolve::{
    FilePanelCandidate, FileTextPresentation, IMAGE_FILE_PANEL, file_text_presentation,
    resolve_file_panel,
};
use serde_json::Value;
use std::sync::Arc;

/// Original plugin `useFileRead`: the file's text or bytes, re-read when it changes on disk,
/// keeping the current value during a re-read. A deletion is an error.
pub struct FileReader<T> {
    generation: u64,
    ready: bool,
    value: Option<T>,
    error: Option<String>,
}

impl<T> FileReader<T> {
    fn new() -> FileReader<T> {
        FileReader { generation: 0, ready: false, value: None, error: None }
    }

    pub fn loading(&self) -> bool {
        !self.ready && self.error.is_none()
    }

    pub fn value(&self) -> Option<&T> {
        if self.ready { self.value.as_ref() } else { None }
    }

    pub fn error(&self) -> Option<&str> {
        if self.ready { None } else { self.error.as_deref() }
    }

    fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn deleted(&mut self) {
        self.generation += 1;
        self.error = Some(WriteError::Deleted.message());
        self.ready = false;
    }

    fn finish(&mut self, generation: u64, result: Result<T, String>) -> bool {
        if generation != self.generation {
            return false;
        }
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.ready = true;
                self.error = None;
            }
            Err(error) => {
                self.ready = false;
                self.error = Some(error);
            }
        }
        true
    }
}

/// Autosave timer requests the store turns into a Makepad timer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AutosaveRequest {
    #[default]
    None,
    Schedule,
    Cancel,
}

struct SessionIo<'a> {
    handle: &'a mut FileHandle,
    autosave: &'a mut AutosaveRequest,
}

impl ModelIo for SessionIo<'_> {
    fn read_text(&mut self, seq: u64, reconcile: bool) {
        let purpose = if reconcile { Purpose::ModelReconcile(seq) } else { Purpose::ModelReload(seq) };
        self.handle.read_text(purpose);
    }
    fn write_text(&mut self, text: String, force: bool, seq: u64) {
        self.handle.write_text(text, force, Purpose::ModelWrite(seq));
    }
    fn flush_text(&mut self, text: String) {
        self.handle.write_text(text, false, Purpose::Flush);
    }
    fn schedule_autosave(&mut self) {
        *self.autosave = AutosaveRequest::Schedule;
    }
    fn cancel_autosave(&mut self) {
        *self.autosave = AutosaveRequest::Cancel;
    }
}

/// The text panel's view (original `useTextFileView`), reset for each panel mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextView {
    Preview,
    Source,
}

pub struct FileSession {
    pub host_id: String,
    pub cwd: String,
    pub path: String,
    pub handle: FileHandle,
    editor_platform: bool,
    panel: Option<FilePanelCandidate>,
    presentation: Option<FileTextPresentation>,
    mounted: Option<String>,
    model: Option<FileEditorModel>,
    text: Option<FileReader<String>>,
    bytes: Option<FileReader<Arc<Vec<u8>>>>,
    pub autosave: AutosaveRequest,
    /// The panel mount the view was last reset for (original `_viewMount`).
    view_mount: Option<String>,
    pub view: TextView,
    /// Bumps on every observable change.
    pub revision: u64,
    /// Released: a modified buffer is still written, then the session goes away.
    pub released: bool,
}

impl FileSession {
    pub fn new(host_id: &str, cwd: &str, path: &str, editor_platform: bool) -> FileSession {
        FileSession {
            host_id: host_id.to_owned(),
            cwd: cwd.to_owned(),
            path: path.to_owned(),
            handle: FileHandle::new(),
            editor_platform,
            panel: None,
            presentation: None,
            mounted: None,
            model: None,
            text: None,
            bytes: None,
            autosave: AutosaveRequest::None,
            view_mount: None,
            view: TextView::Source,
            revision: 0,
            released: false,
        }
    }

    pub fn status(&self) -> Snapshot {
        self.handle.snapshot()
    }

    pub fn stat(&self) -> Option<&FileVersion> {
        self.handle.stat()
    }

    /// The panel the ready file resolves to; `None` before the stat and for binary files.
    pub fn panel(&self) -> Option<FilePanelCandidate> {
        self.panel
    }

    pub fn presentation(&self) -> Option<FileTextPresentation> {
        self.presentation
    }

    pub fn model(&self) -> Option<&FileEditorModel> {
        self.model.as_ref()
    }

    pub fn text(&self) -> Option<&FileReader<String>> {
        self.text.as_ref()
    }

    pub fn bytes(&self) -> Option<&FileReader<Arc<Vec<u8>>>> {
        self.bytes.as_ref()
    }

    /// Whether the open buffer has unsaved changes (the tab's `modified`).
    pub fn modified(&self) -> bool {
        self.model.as_ref().is_some_and(FileEditorModel::modified)
    }

    /// The mount key `panel:presentation` the text view resets on.
    pub fn mount_key(&self) -> Option<String> {
        self.panel.map(|panel| {
            format!("{}:{}", panel.id, self.presentation.map(|p| p.name()).unwrap_or(""))
        })
    }

    /// The text the mounted text panel shows, once read.
    pub fn current_text(&self) -> Option<&str> {
        if let Some(model) = &self.model {
            use crate::features::files::model::FileEditorStatus::{Failed, Loading};
            return match model.status() {
                Loading | Failed => None,
                _ => Some(model.content()),
            };
        }
        self.text.as_ref().and_then(|reader| reader.value().map(String::as_str))
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Applies `f` to the model with this session's IO, then routes what it caused.
    pub fn with_model(&mut self, associations: &Value, f: impl FnOnce(&mut FileEditorModel, &mut dyn ModelIo)) {
        if let Some(model) = self.model.as_mut() {
            let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
            f(model, &mut io);
            self.changed();
        }
        self.pump(associations);
    }

    /// Re-reads a failed text/image read (the panels' Retry).
    pub fn retry_read(&mut self, associations: &Value) {
        if let Some(reader) = self.text.as_mut() {
            let generation = reader.begin();
            self.handle.read_text(Purpose::TextReader(generation));
        }
        if let Some(reader) = self.bytes.as_mut() {
            let generation = reader.begin();
            self.handle.read_bytes(Purpose::BytesReader(generation));
        }
        self.pump(associations);
    }

    /// Routes handle events and job outcomes until nothing is left.
    pub fn pump(&mut self, associations: &Value) {
        loop {
            let events = std::mem::take(&mut self.handle.events);
            let outcomes = std::mem::take(&mut self.handle.outcomes);
            if events.is_empty() && outcomes.is_empty() {
                return;
            }
            self.changed();
            for event in events {
                match event {
                    HandleEvent::Snapshot => self.on_handle(associations),
                    HandleEvent::Changed => self.on_file_event(FileEvent::Changed),
                    HandleEvent::Deleted => self.on_file_event(FileEvent::Deleted),
                }
            }
            for outcome in outcomes {
                self.on_outcome(outcome);
            }
        }
    }

    fn on_file_event(&mut self, event: FileEvent) {
        if let Some(model) = self.model.as_mut() {
            let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
            model.on_file_event(event, &mut io);
        }
        if let Some(reader) = self.text.as_mut() {
            match event {
                FileEvent::Changed => {
                    let generation = reader.begin();
                    self.handle.read_text(Purpose::TextReader(generation));
                }
                FileEvent::Deleted => reader.deleted(),
            }
        }
        if let Some(reader) = self.bytes.as_mut() {
            match event {
                FileEvent::Changed => {
                    let generation = reader.begin();
                    self.handle.read_bytes(Purpose::BytesReader(generation));
                }
                FileEvent::Deleted => reader.deleted(),
            }
        }
    }

    fn on_outcome(&mut self, outcome: Outcome) {
        match outcome.purpose {
            Purpose::ModelReload(seq) | Purpose::ModelReconcile(seq) => {
                let reconcile = matches!(outcome.purpose, Purpose::ModelReconcile(_));
                let result = match outcome.result {
                    JobResult::Text(text) => Ok(text),
                    JobResult::ReadFailed(error) => Err(error),
                    _ => return,
                };
                if let Some(model) = self.model.as_mut() {
                    let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
                    model.on_read(seq, reconcile, result, &mut io);
                }
            }
            Purpose::ModelWrite(seq) => {
                let result = match outcome.result {
                    JobResult::Written => Ok(()),
                    JobResult::WriteFailed(WriteError::Conflict) => Err(WriteFailure::Conflict),
                    JobResult::WriteFailed(WriteError::Deleted) => Err(WriteFailure::Deleted),
                    JobResult::WriteFailed(error) => Err(WriteFailure::Other(error.message())),
                    _ => return,
                };
                if let Some(model) = self.model.as_mut() {
                    let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
                    model.on_write(seq, result, &mut io);
                }
            }
            Purpose::Flush => {}
            Purpose::TextReader(generation) => {
                if let Some(reader) = self.text.as_mut() {
                    let result = match outcome.result {
                        JobResult::Text(text) => Ok(text),
                        JobResult::ReadFailed(error) => Err(error),
                        _ => return,
                    };
                    reader.finish(generation, result);
                }
            }
            Purpose::BytesReader(generation) => {
                if let Some(reader) = self.bytes.as_mut() {
                    let result = match outcome.result {
                        JobResult::Bytes(bytes) => Ok(bytes),
                        JobResult::ReadFailed(error) => Err(error),
                        _ => return,
                    };
                    reader.finish(generation, result);
                }
            }
        }
    }

    /// The handle's snapshot changed: resolve the panel and mount its file state.
    fn on_handle(&mut self, associations: &Value) {
        let Snapshot::Ready(stat) = self.handle.snapshot() else { return };
        let resolution = resolve_file_panel(
            &self.path,
            stat.kind().unwrap_or("binary"),
            associations,
            None,
        );
        let panel = resolution.panel;
        let presentation = match panel {
            Some(panel) if panel.file_kind == "text" => {
                Some(file_text_presentation(stat.size(), self.editor_platform))
            }
            _ => None,
        };
        self.panel = panel;
        self.presentation = presentation;
        let editable = presentation == Some(FileTextPresentation::Editable);
        let readable = matches!(
            presentation,
            Some(FileTextPresentation::Highlighted | FileTextPresentation::Plain)
        );
        let mount = panel.map(|panel| {
            let mode = if editable {
                "edit"
            } else if readable {
                "read"
            } else {
                presentation.map(|p| p.name()).unwrap_or("")
            };
            format!("{}:{mode}", panel.id)
        });
        if mount == self.mounted {
            return;
        }
        self.unmount();
        self.mounted = mount;
        if panel == Some(IMAGE_FILE_PANEL) {
            let mut reader = FileReader::new();
            let generation = reader.begin();
            self.bytes = Some(reader);
            self.handle.read_bytes(Purpose::BytesReader(generation));
        } else if editable {
            let mut model = FileEditorModel::new();
            let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
            model.open(&mut io);
            self.model = Some(model);
        } else if readable {
            let mut reader = FileReader::new();
            let generation = reader.begin();
            self.text = Some(reader);
            self.handle.read_text(Purpose::TextReader(generation));
        }
    }

    /// One panel mount ends: a modified buffer is written first, so switching panels keeps the
    /// edit.
    fn unmount(&mut self) {
        if let Some(mut model) = self.model.take() {
            let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
            model.close(true, &mut io);
        }
        self.text = None;
        self.bytes = None;
    }

    /// Resets the text view for a new panel mount: Preview by default, Source for a line target.
    /// Returns whether a reset happened.
    pub fn sync_view_mount(&mut self, line_target: bool) -> bool {
        let mount = self.mount_key();
        if mount == self.view_mount {
            return false;
        }
        self.view_mount = mount;
        self.view = if line_target { TextView::Source } else { TextView::Preview };
        true
    }

    /// The tab moved to another file or closed after its owner went away: a modified buffer is
    /// still written to this file, and the handle closes once that write lands or fails.
    pub fn release(&mut self, associations: &Value) {
        if self.released {
            return;
        }
        self.released = true;
        self.handle.set_observing(false);
        self.unmount();
        self.pump(associations);
    }

    /// Closing the tab after "close without saving": held and queued writes are dropped.
    pub fn dispose(&mut self) {
        if let Some(mut model) = self.model.take() {
            let mut io = SessionIo { handle: &mut self.handle, autosave: &mut self.autosave };
            model.close(false, &mut io);
        }
        self.text = None;
        self.bytes = None;
        self.handle.dispose();
        self.released = true;
    }

    /// A released session whose writes settled can go away.
    pub fn finished(&self) -> bool {
        self.released && (self.handle.is_disposed() || self.handle.settled())
    }
}
