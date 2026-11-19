//! UITextView is the native UITextInput document. Rust owns the visible editor;
//! UIKit owns marked text, tokenizer, selection and native replacement behavior.
use super::{
    ios_app::{get_ios_class_global, IosApp},
    ios_event::IosEvent,
    ios_state::{accepts_edit, utf16_selection, TextSession},
};
use crate::{
    area::Area,
    event::TextImeEvent,
    makepad_math::Rect,
    os::apple::{apple_sys::*, apple_util::str_to_nsstring},
};

const SESSION: &str = "makepadSession";
const MUTATION_DEPTH: &str = "makepadMutationDepth";
const SINGLE_LINE: &str = "makepadSingleLine";

pub(super) fn define_text_view() -> *const Class {
    let mut decl = ClassDecl::new("MakepadTextInputView", class!(UITextView)).unwrap();
    decl.add_ivar::<u64>(SESSION);
    decl.add_ivar::<u32>(MUTATION_DEPTH);
    decl.add_ivar::<BOOL>(SINGLE_LINE);

    // Do not accept pointer events or expose a second accessible editor. The
    // view remains in the key window, so it can take first responder normally.
    extern "C" fn point_inside(_: &Object, _: Sel, _: NSPoint, _: ObjcId) -> BOOL { NO }

    extern "C" fn mark(this: &mut Object, _: Sel, text: ObjcId, selection: NSRange) {
        unsafe { edit(this, |view| msg_send![super (view, class!(UITextView)), setMarkedText: text selectedRange: selection]); }
    }
    extern "C" fn unmark(this: &mut Object, _: Sel) {
        unsafe { edit(this, |view| msg_send![super (view, class!(UITextView)), unmarkText]); }
    }
    extern "C" fn insert(this: &mut Object, _: Sel, text: ObjcId) {
        unsafe {
            if *this.get_ivar::<u32>(MUTATION_DEPTH) == 0 && *this.get_ivar::<BOOL>(SINGLE_LINE) == YES {
                let session = *this.get_ivar::<u64>(SESSION);
                let view = std::ptr::from_mut(this);
                if session != 0 && is_return(view, text) {
                    IosApp::defer_callback(IosEvent::TextSubmit(session));
                    return;
                }
            }
        }
        unsafe { edit(this, |view| msg_send![super (view, class!(UITextView)), insertText: text]); }
    }
    extern "C" fn delete(this: &mut Object, _: Sel) {
        unsafe { edit(this, |view| msg_send![super (view, class!(UITextView)), deleteBackward]); }
    }
    extern "C" fn replace(this: &mut Object, _: Sel, range: ObjcId, text: ObjcId) {
        unsafe { edit(this, |view| msg_send![super (view, class!(UITextView)), replaceRange: range withText: text]); }
    }
    extern "C" fn paste(this: &mut Object, _: Sel, sender: ObjcId) {
        unsafe { edit(this, |view| msg_send![super (view, class!(UITextView)), paste: sender]); }
    }

    unsafe {
        decl.add_method(sel!(pointInside: withEvent:), point_inside as extern "C" fn(&Object, Sel, NSPoint, ObjcId) -> BOOL);
        decl.add_method(sel!(setMarkedText: selectedRange:), mark as extern "C" fn(&mut Object, Sel, ObjcId, NSRange));
        decl.add_method(sel!(unmarkText), unmark as extern "C" fn(&mut Object, Sel));
        decl.add_method(sel!(insertText:), insert as extern "C" fn(&mut Object, Sel, ObjcId));
        decl.add_method(sel!(deleteBackward), delete as extern "C" fn(&mut Object, Sel));
        decl.add_method(sel!(replaceRange: withText:), replace as extern "C" fn(&mut Object, Sel, ObjcId, ObjcId));
        decl.add_method(sel!(paste:), paste as extern "C" fn(&mut Object, Sel, ObjcId));
    }
    decl.register()
}

// UIKit can send text and selection delegate callbacks during the same edit,
// before it publishes the new markedTextRange. Snapshot after the outer call.
unsafe fn edit(this: &mut Object, change: impl FnOnce(ObjcId)) {
    let view = std::ptr::from_mut(this);
    let depth = *this.get_ivar::<u32>(MUTATION_DEPTH);
    this.set_ivar(MUTATION_DEPTH, depth + 1);
    change(view);
    (*view).set_ivar(MUTATION_DEPTH, depth);
    if depth == 0 { report(view, false); }
}

pub(super) extern "C" fn text_changed(_: &Object, _: Sel, view: ObjcId) {
    unsafe { report(view, false); }
}

pub(super) extern "C" fn text_ended(_: &Object, _: Sel, view: ObjcId) {
    unsafe { report(view, true); }
}

unsafe fn report(view: ObjcId, ended: bool) {
    let session = *(*view).get_ivar::<u64>(SESSION);
    if session == 0 || *(*view).get_ivar::<u32>(MUTATION_DEPTH) != 0 { return; }
    let first: BOOL = msg_send![view, isFirstResponder];
    if !ended && first == NO { return; }
    let string: ObjcId = msg_send![view, text];
    // NSString conversion can fail if a native edit supplied malformed UTF-16.
    if string == nil { return; }
    let bytes: *const u8 = msg_send![string, UTF8String];
    if bytes.is_null() { return; }
    let length: usize = msg_send![string, lengthOfBytesUsingEncoding: UTF8_ENCODING];
    let Ok(text) = std::str::from_utf8(std::slice::from_raw_parts(bytes, length)) else { return; };
    let selected: NSRange = msg_send![view, selectedRange];
    let len: usize = msg_send![string, length];
    let start = (selected.location as usize).min(len);
    let end = (selected.location.saturating_add(selected.length) as usize).min(len);
    let mark = if ended { None } else { marked_range(view).filter(|&(start, end)| start < end && end <= len) };
    let event = IosEvent::TextEdit {
        session, text: text.to_owned(), selection: (start, end), composing: mark, ended,
    };
    if ended { IosApp::defer_callback(event); } else { IosApp::do_callback(event); }
}

unsafe fn marked_range(view: ObjcId) -> Option<(usize, usize)> {
    let range: ObjcId = msg_send![view, markedTextRange];
    if range == nil { return None; }
    let beginning: ObjcId = msg_send![view, beginningOfDocument];
    let start: ObjcId = msg_send![range, start];
    let end: ObjcId = msg_send![range, end];
    let start: isize = msg_send![view, offsetFromPosition: beginning toPosition: start];
    let end: isize = msg_send![view, offsetFromPosition: beginning toPosition: end];
    (start >= 0 && end >= start).then_some((start as usize, end as usize))
}

unsafe fn is_return(view: ObjcId, text: ObjcId) -> bool {
    let length: usize = msg_send![text, length];
    if length != 1 { return false; }
    let ch: u16 = msg_send![text, characterAtIndex: 0usize];
    // A Return that is still inside a composition belongs to UIKit. Pasting
    // a newline or an explicit replaceRange: call is an edit, not submission.
    let mark: ObjcId = msg_send![view, markedTextRange];
    ch == u16::from(b'\n') && mark == nil
}

#[derive(Default)]
pub(super) struct TextInput {
    next_session: u64,
    active: Option<Editor>,
}

struct Editor {
    native: ObjcId, // owns an alloc retain
    area: Area,
    id: u64,
    traits: (bool, bool, bool, bool),
    session: TextSession,
    delivered: u64,
    caret: Option<Rect>,
}

impl TextInput {
    pub fn area(&self) -> Area {
        self.active.as_ref().map_or(Area::Empty, |editor| editor.area)
    }

    pub fn publish(&mut self, area: Area, id: u64, text: &str, (start, end): (usize, usize),
        traits: (bool, bool, bool, bool)) -> Option<TextImeEvent> {
        let selection = utf16_selection(text, start, end);
        if let Some(editor) = self.active.as_mut().filter(|editor| editor.id == id && editor.traits == traits) {
            editor.area = area;
            let (text_changed, interrupted) = editor.session.publish(text, selection)?;
            editor.caret = None;
            unsafe { sync_document(editor.native, text_changed.then_some(text), selection, interrupted); }
            return interrupted.then(|| editor.session.snapshot());
        }

        let Some((host, delegate)) = IosApp::text_input_host() else { return None; };
        self.next_session += 1;
        let session = TextSession::new(self.next_session, text, selection);
        let (password, numeric, terminal, single_line) = traits;
        unsafe {
            let native: ObjcId = msg_send![get_ios_class_global().text_view, alloc];
            let frame = NSRect { origin: NSPoint { x: 0., y: 0. }, size: NSSize { width: 128., height: 40. } };
            let native: ObjcId = msg_send![native, initWithFrame: frame textContainer: nil];
            (*native).set_ivar(MUTATION_DEPTH, 1u32);
            (*native).set_ivar(SINGLE_LINE, if single_line { YES } else { NO });
            let layer: ObjcId = msg_send![native, layer];
            let () = msg_send![layer, setOpacity: 0f32];
            let () = msg_send![native, setIsAccessibilityElement: NO];
            let () = msg_send![native, setAccessibilityElementsHidden: YES];
            let () = msg_send![native, setAutocapitalizationType: 0isize]; // None
            if terminal || password || numeric {
                let () = msg_send![native, setAutocorrectionType: 1isize]; // No
                let () = msg_send![native, setSpellCheckingType: 1isize]; // No
                let () = msg_send![native, setSmartDashesType: 1isize]; // No
                let () = msg_send![native, setSmartQuotesType: 1isize]; // No
                let () = msg_send![native, setSmartInsertDeleteType: 1isize]; // No
            }
            if password { let () = msg_send![native, setSecureTextEntry: YES]; }
            if numeric { let () = msg_send![native, setKeyboardType: 2isize]; } // NumbersAndPunctuation
            if single_line { let () = msg_send![native, setReturnKeyType: 9isize]; } // Done
            let () = msg_send![native, setDelegate: delegate];
            let () = msg_send![host, addSubview: native];
            sync_document(native, Some(text), selection, false);
            let session_id = session.id;
            let old = self.active.replace(Editor { native, area, id, traits, session, delivered: 0, caret: None });
            if let Some(old) = old {
                // Fence any old UIKit callbacks before handing the native keyboard off.
                (*old.native).set_ivar(SESSION, 0u64);
                let was_first: BOOL = msg_send![old.native, isFirstResponder];
                if was_first == YES { let _: BOOL = msg_send![native, becomeFirstResponder]; }
                drop(old);
            }
            (*native).set_ivar(SESSION, session_id);
            (*native).set_ivar(MUTATION_DEPTH, 0u32);
        }
        None
    }

    pub fn observe(&mut self, session: u64, text: String, selection: (usize, usize), composing: Option<(usize, usize)>) -> Option<TextImeEvent> {
        let editor = self.active.as_mut().filter(|editor| editor.session.id == session)?;
        editor.session.observe(text, selection, composing)
    }

    pub fn did_end_editing(&self, session: u64) -> bool {
        self.active.as_ref().is_some_and(|editor| unsafe {
            if editor.session.id != session { return false; }
            let first: BOOL = msg_send![editor.native, isFirstResponder];
            first == NO
        })
    }

    pub fn admit(&mut self, edit: &TextImeEvent) -> bool {
        let Some(editor) = &mut self.active else { return false; };
        if !accepts_edit(editor.session.id, editor.delivered, edit.session, edit.revision) { return false; }
        editor.delivered = edit.revision;
        true
    }

    pub fn can_submit(&self, session: u64) -> bool {
        self.active.as_ref().is_some_and(|editor| editor.session.id == session && editor.traits.3 && editor.session.composing.is_none())
    }

    pub fn show(&mut self, caret: Rect) {
        let Some(editor) = &mut self.active else { return; };
        unsafe {
            if editor.caret != Some(caret) {
                editor.caret = Some(caret);
                let frame = NSRect { origin: NSPoint { x: caret.pos.x, y: caret.pos.y },
                    size: NSSize { width: 128., height: caret.size.y.max(24.) } };
                let () = msg_send![editor.native, setFrame: frame];
            }
            let first: BOOL = msg_send![editor.native, isFirstResponder];
            if first == NO { let _: BOOL = msg_send![editor.native, becomeFirstResponder]; }
        }
    }

    pub fn hide(&mut self) -> Option<TextImeEvent> {
        let editor = self.active.as_mut()?;
        let ended = editor.end_composition();
        unsafe {
            let native = editor.native;
            let depth = *(*native).get_ivar::<u32>(MUTATION_DEPTH);
            (*native).set_ivar(MUTATION_DEPTH, depth + 1);
            let _: BOOL = msg_send![native, resignFirstResponder];
            (*native).set_ivar(MUTATION_DEPTH, depth);
        }
        ended
    }

    pub fn end_composition(&mut self) -> Option<TextImeEvent> {
        self.active.as_mut()?.end_composition()
    }

    pub fn clear(&mut self, area: Area) {
        if self.area() == area { self.active = None; }
    }
}

impl Editor {
    fn end_composition(&mut self) -> Option<TextImeEvent> {
        self.session.composing.take()?;
        unsafe { sync_document(self.native, None, self.session.selection, true); }
        Some(self.session.snapshot())
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            let native = self.native;
            (*native).set_ivar(SESSION, 0u64);
            let () = msg_send![native, setDelegate: nil];
            let _: BOOL = msg_send![native, resignFirstResponder];
            let () = msg_send![native, removeFromSuperview];
            let () = msg_send![native, release];
        }
    }
}

unsafe fn sync_document(native: ObjcId, text: Option<&str>, (start, end): (usize, usize), unmark: bool) {
    let depth = *(*native).get_ivar::<u32>(MUTATION_DEPTH);
    (*native).set_ivar(MUTATION_DEPTH, depth + 1);
    if unmark { let () = msg_send![native, unmarkText]; }
    if let Some(text) = text { let () = msg_send![native, setText: str_to_nsstring(text)]; }
    let range = NSRange { location: start as u64, length: (end - start) as u64 };
    let () = msg_send![native, setSelectedRange: range];
    (*native).set_ivar(MUTATION_DEPTH, depth);
}
