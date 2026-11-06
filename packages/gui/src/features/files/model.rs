//! Source's buffer over a file (Flutter `file_editor_model.dart`, original
//! `file-panels/source/editor/model.ts`): reads the file, autosaves edits, and reconciles
//! changes on disk. A clean buffer follows the disk; a modified one conflicts and the user
//! overwrites or reloads. The buffer holds `\n` line breaks; writes restore the file's detected
//! line separator.
//!
//! Sans-IO: the model asks its [`ModelIo`] (the session) to read, write and (re)arm the autosave
//! timer, and hears results through `on_*`.

/// Original `FileEditorStatus`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileEditorStatus {
    Loading,
    Failed,
    Clean,
    Dirty,
    Saving,
    Conflict,
    Error,
}

/// Original `AUTOSAVE_DELAY_MS`.
pub const AUTOSAVE_DELAY_SECONDS: f64 = 0.8;

/// `changed` | `deleted` while in conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Conflict {
    Changed,
    Deleted,
}

/// What the model needs from the file it edits.
pub trait ModelIo {
    /// Reads the file's text; the result returns through `on_read(seq, ...)`.
    fn read_text(&mut self, seq: u64, reconcile: bool);
    /// Writes `text` (already in the file's line separators); returns through `on_write(seq, ...)`.
    fn write_text(&mut self, text: String, force: bool, seq: u64);
    /// A fire-and-forget write of a modified buffer whose panel unmounts.
    fn flush_text(&mut self, text: String);
    fn schedule_autosave(&mut self);
    fn cancel_autosave(&mut self);
}

/// A failed write, as the handle reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteFailure {
    Conflict,
    Deleted,
    Other(String),
}

/// Original `detectLineSeparator`: the file's first line break, `\n` without one.
pub fn detect_line_separator(content: &str) -> &'static str {
    let bytes = content.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'\n' => return "\n",
            b'\r' => {
                return if bytes.get(index + 1) == Some(&b'\n') { "\r\n" } else { "\r" };
            }
            _ => {}
        }
    }
    "\n"
}

pub struct FileEditorModel {
    status: FileEditorStatus,
    content: String,
    line_separator: &'static str,
    modified: bool,
    conflict: Option<Conflict>,
    error: Option<String>,
    persisted: String,
    saving: bool,
    /// The buffer of the write in flight, by save sequence (original `final content = _content`).
    saving_content: Option<(u64, String)>,
    save_sequence: u64,
    read_sequence: u64,
    event_during_save: Option<FileEvent>,
    watching: bool,
    disposed: bool,
    /// Bumps on every observable change (the views' redraw/sync key).
    pub revision: u64,
}

/// A change on disk, as the handle reports it to watchers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileEvent {
    Changed,
    Deleted,
}

impl Default for FileEditorModel {
    fn default() -> Self {
        FileEditorModel {
            status: FileEditorStatus::Loading,
            content: String::new(),
            line_separator: "\n",
            modified: false,
            conflict: None,
            error: None,
            persisted: String::new(),
            saving: false,
            saving_content: None,
            save_sequence: 0,
            read_sequence: 0,
            event_during_save: None,
            watching: false,
            disposed: false,
            revision: 0,
        }
    }
}

impl FileEditorModel {
    pub fn new() -> FileEditorModel {
        FileEditorModel::default()
    }

    pub fn status(&self) -> FileEditorStatus {
        self.status
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    /// The buffer differs from what this editor last read or saved.
    pub fn modified(&self) -> bool {
        self.modified
    }

    pub fn conflict(&self) -> Option<Conflict> {
        self.conflict
    }

    /// The failed load's or save's message.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The buffer as the file stores it.
    pub fn file_content(&self) -> String {
        self.with_separator(&self.content)
    }

    fn is_loaded(&self) -> bool {
        !matches!(self.status, FileEditorStatus::Loading | FileEditorStatus::Failed)
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Reads the file and follows its changes on disk.
    pub fn open(&mut self, io: &mut dyn ModelIo) {
        if self.disposed || self.watching {
            return;
        }
        self.watching = true;
        self.reload(io);
    }

    pub fn edit(&mut self, content: String, io: &mut dyn ModelIo) {
        if self.disposed || !self.is_loaded() || content == self.content {
            return;
        }
        let modified = content != self.persisted;
        let mut status = if modified { FileEditorStatus::Dirty } else { FileEditorStatus::Clean };
        if self.status == FileEditorStatus::Conflict {
            status = FileEditorStatus::Conflict;
        }
        self.status = status;
        self.content = content;
        self.modified = modified;
        self.error = None;
        self.changed();
        if status == FileEditorStatus::Dirty {
            io.schedule_autosave();
        } else {
            io.cancel_autosave();
        }
    }

    pub fn save(&mut self, io: &mut dyn ModelIo) {
        if !matches!(self.status, FileEditorStatus::Dirty | FileEditorStatus::Error) {
            return;
        }
        self.write(false, io);
    }

    /// Writes the buffer over a file that changed on disk.
    pub fn overwrite(&mut self, io: &mut dyn ModelIo) {
        if self.conflict != Some(Conflict::Changed) {
            return;
        }
        self.write(true, io);
    }

    /// Replaces the buffer with the file on disk.
    pub fn reload(&mut self, io: &mut dyn ModelIo) {
        if self.disposed {
            return;
        }
        self.read_sequence += 1;
        io.read_text(self.read_sequence, false);
    }

    /// The autosave timer fired.
    pub fn autosave(&mut self, io: &mut dyn ModelIo) {
        self.save(io);
    }

    /// Stops following the file. With `flush`, a modified buffer is written first.
    pub fn close(&mut self, flush: bool, io: &mut dyn ModelIo) {
        if self.disposed {
            return;
        }
        if flush && self.modified && self.status != FileEditorStatus::Conflict {
            io.flush_text(self.file_content());
        }
        self.disposed = true;
        self.watching = false;
        io.cancel_autosave();
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    pub fn on_file_event(&mut self, event: FileEvent, io: &mut dyn ModelIo) {
        if self.disposed || !self.watching {
            return;
        }
        if !self.is_loaded() {
            if event == FileEvent::Changed {
                self.reload(io);
            }
            return;
        }
        if self.saving {
            self.event_during_save = Some(event);
            return;
        }
        if event == FileEvent::Deleted {
            self.enter_conflict(Conflict::Deleted, io);
            return;
        }
        self.read_sequence += 1;
        io.read_text(self.read_sequence, true);
    }

    /// A read the model asked for finished.
    pub fn on_read(&mut self, seq: u64, reconcile: bool, result: Result<String, String>, io: &mut dyn ModelIo) {
        if self.disposed || seq != self.read_sequence {
            return;
        }
        if reconcile {
            // Follows a clean buffer to the disk; a modified one conflicts unless the disk
            // matches it. A failed read changes nothing.
            let Ok(content) = result else { return };
            if self.saving {
                self.event_during_save = Some(FileEvent::Changed);
                return;
            }
            if content == self.with_separator(&self.persisted) {
                self.adopt_unchanged_file(io);
                return;
            }
            if !self.modified {
                self.apply_file(&content, io);
                return;
            }
            self.enter_conflict(Conflict::Changed, io);
            return;
        }
        match result {
            Ok(content) => self.apply_file(&content, io),
            Err(error) => {
                if self.is_loaded() {
                    return;
                }
                self.status = FileEditorStatus::Failed;
                self.error = Some(error);
                self.changed();
            }
        }
    }

    fn write(&mut self, force: bool, io: &mut dyn ModelIo) {
        if self.disposed {
            return;
        }
        io.cancel_autosave();
        self.save_sequence += 1;
        self.saving = true;
        self.event_during_save = None;
        self.status = FileEditorStatus::Saving;
        self.conflict = None;
        self.error = None;
        self.changed();
        // The content being saved is remembered by sequence: the buffer may change meanwhile.
        self.saving_content = Some((self.save_sequence, self.content.clone()));
        io.write_text(self.with_separator(&self.content), force, self.save_sequence);
    }

    /// A write the model asked for finished.
    pub fn on_write(&mut self, seq: u64, result: Result<(), WriteFailure>, io: &mut dyn ModelIo) {
        let saved = self.saving_content.take_if(|(sequence, _)| *sequence == seq);
        if self.disposed || seq != self.save_sequence {
            self.saving = false;
            return;
        }
        self.saving = false;
        match result {
            Err(WriteFailure::Conflict) => self.enter_conflict(Conflict::Changed, io),
            Err(WriteFailure::Deleted) => self.enter_conflict(Conflict::Deleted, io),
            Err(WriteFailure::Other(error)) => {
                self.status = FileEditorStatus::Error;
                self.error = Some(error);
                self.changed();
            }
            Ok(()) => {
                if let Some((_, content)) = saved {
                    self.persisted = content;
                }
                self.modified = self.content != self.persisted;
                self.status =
                    if self.modified { FileEditorStatus::Dirty } else { FileEditorStatus::Clean };
                self.changed();
                if self.modified {
                    io.schedule_autosave();
                }
            }
        }
        if let Some(event) = self.event_during_save.take() {
            self.on_file_event(event, io);
        }
    }

    fn apply_file(&mut self, raw: &str, io: &mut dyn ModelIo) {
        io.cancel_autosave();
        self.save_sequence += 1;
        self.line_separator = detect_line_separator(raw);
        let content = normalize(raw);
        self.persisted = content.clone();
        self.status = FileEditorStatus::Clean;
        self.content = content;
        self.modified = false;
        self.conflict = None;
        self.error = None;
        self.changed();
    }

    fn adopt_unchanged_file(&mut self, io: &mut dyn ModelIo) {
        if self.status != FileEditorStatus::Conflict {
            return;
        }
        self.status = if self.modified { FileEditorStatus::Dirty } else { FileEditorStatus::Clean };
        self.conflict = None;
        self.error = None;
        self.changed();
        if self.status == FileEditorStatus::Dirty {
            io.schedule_autosave();
        }
    }

    fn enter_conflict(&mut self, conflict: Conflict, io: &mut dyn ModelIo) {
        io.cancel_autosave();
        self.status = FileEditorStatus::Conflict;
        self.conflict = Some(conflict);
        self.error = None;
        self.changed();
    }

    fn with_separator(&self, content: &str) -> String {
        if self.line_separator == "\n" {
            content.to_owned()
        } else {
            content.replace('\n', self.line_separator)
        }
    }
}

fn normalize(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk the tests drive: reads and writes complete when `run` is called.
    #[derive(Default)]
    struct Io {
        disk: String,
        revision: u32,
        base: u32,
        exists: bool,
        reads: Vec<(u64, bool)>,
        writes: Vec<(String, bool, u64)>,
        autosave: bool,
    }

    impl ModelIo for Io {
        fn read_text(&mut self, seq: u64, reconcile: bool) {
            self.reads.push((seq, reconcile));
        }
        fn write_text(&mut self, text: String, force: bool, seq: u64) {
            self.writes.push((text, force, seq));
        }
        fn flush_text(&mut self, text: String) {
            self.disk = text;
        }
        fn schedule_autosave(&mut self) {
            self.autosave = true;
        }
        fn cancel_autosave(&mut self) {
            self.autosave = false;
        }
    }

    fn run(model: &mut FileEditorModel, io: &mut Io) {
        loop {
            let reads = std::mem::take(&mut io.reads);
            let writes = std::mem::take(&mut io.writes);
            if reads.is_empty() && writes.is_empty() {
                return;
            }
            for (seq, reconcile) in reads {
                let result = if io.exists { Ok(io.disk.clone()) } else { Err("missing".to_owned()) };
                if result.is_ok() {
                    io.base = io.revision;
                }
                model.on_read(seq, reconcile, result, io);
            }
            for (text, force, seq) in writes {
                let result = if !io.exists {
                    Err(WriteFailure::Deleted)
                } else if io.base != io.revision && !force {
                    Err(WriteFailure::Conflict)
                } else {
                    io.disk = text;
                    io.revision += 1;
                    io.base = io.revision;
                    Ok(())
                };
                model.on_write(seq, result, io);
            }
        }
    }

    fn open(text: &str) -> (FileEditorModel, Io) {
        let mut io = Io { disk: text.to_owned(), exists: true, ..Io::default() };
        let mut model = FileEditorModel::new();
        model.open(&mut io);
        run(&mut model, &mut io);
        (model, io)
    }

    fn change_on_disk(model: &mut FileEditorModel, io: &mut Io, text: &str) {
        io.disk = text.to_owned();
        io.revision += 1;
        model.on_file_event(FileEvent::Changed, io);
        run(model, io);
    }

    #[test]
    fn edits_autosave_and_return_to_clean() {
        let (mut model, mut io) = open("a");
        assert_eq!(model.status(), FileEditorStatus::Clean);
        model.edit("ab".into(), &mut io);
        assert_eq!(model.status(), FileEditorStatus::Dirty);
        assert!(io.autosave);
        model.autosave(&mut io);
        run(&mut model, &mut io);
        assert_eq!(io.disk, "ab");
        assert_eq!(model.status(), FileEditorStatus::Clean);
        assert!(!model.modified());
    }

    #[test]
    fn clean_buffer_follows_the_disk_modified_conflicts() {
        let (mut model, mut io) = open("a");
        change_on_disk(&mut model, &mut io, "b");
        assert_eq!(model.content(), "b");
        assert_eq!(model.status(), FileEditorStatus::Clean);
        model.edit("mine".into(), &mut io);
        change_on_disk(&mut model, &mut io, "theirs");
        assert_eq!(model.status(), FileEditorStatus::Conflict);
        assert_eq!(model.conflict(), Some(Conflict::Changed));
        assert_eq!(model.content(), "mine");
        model.overwrite(&mut io);
        run(&mut model, &mut io);
        assert_eq!(io.disk, "mine");
        assert_eq!(model.status(), FileEditorStatus::Clean);
    }

    #[test]
    fn deleted_file_keeps_the_buffer_in_a_deleted_conflict() {
        let (mut model, mut io) = open("a");
        model.edit("mine".into(), &mut io);
        io.exists = false;
        model.on_file_event(FileEvent::Deleted, &mut io);
        assert_eq!(model.status(), FileEditorStatus::Conflict);
        assert_eq!(model.conflict(), Some(Conflict::Deleted));
        assert_eq!(model.content(), "mine");
    }

    #[test]
    fn saving_over_a_changed_version_conflicts() {
        let (mut model, mut io) = open("a");
        model.edit("b".into(), &mut io);
        io.revision += 1;
        model.save(&mut io);
        run(&mut model, &mut io);
        assert_eq!(model.status(), FileEditorStatus::Conflict);
        assert_eq!(io.disk, "a");
    }

    #[test]
    fn line_separators_are_restored_on_save() {
        let (mut model, mut io) = open("a\r\nb");
        assert_eq!(model.content(), "a\nb");
        model.edit("a\nb\nc".into(), &mut io);
        model.autosave(&mut io);
        run(&mut model, &mut io);
        assert_eq!(io.disk, "a\r\nb\r\nc");
    }

    #[test]
    fn flush_on_close_writes_a_modified_buffer() {
        let (mut model, mut io) = open("a");
        model.edit("unsaved".into(), &mut io);
        model.close(true, &mut io);
        assert_eq!(io.disk, "unsaved");
    }
}
