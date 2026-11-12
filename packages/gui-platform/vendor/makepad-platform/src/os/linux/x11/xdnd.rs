//! Paseo: the XDND drop target (protocol version 5) of every Xlib window.
//!
//! Upstream announced `XdndAware`, answered every `XdndPosition` with "accepted,
//! XdndActionPrivate" and asked for the selection on `XdndDrop`, but never read it, never
//! sent `XdndFinished` and never told the application: no file drop reached a widget, and
//! the source was left waiting. A compositor bridging a Wayland drag to an X11 window maps
//! the private action to "no action" and never delivered the drop at all.
//!
//! The protocol steps:
//!
//! 1. `XdndEnter` names the source, its protocol version and the offered types.
//! 2. The first `XdndPosition` of a drag that offers `text/uri-list` requests the list, so
//!    widgets know the files while the pointer moves. Until it arrives the drag is accepted
//!    provisionally; from then on every position is a [`DragEvent`](crate::event::DragEvent)
//!    and the widgets' [`DragResponse`] is the `XdndStatus` (refused positions show the
//!    source's "no drop" cursor, and the source then sends `XdndLeave`, not `XdndDrop`).
//! 3. `XdndDrop` becomes a [`DropEvent`](crate::event::DropEvent); `XdndFinished` reports
//!    whether a widget took it. A source that only answers the selection after the drop is
//!    asked then.
//! 4. `XdndLeave` (and the end of a drop) is `Event::DragEnd`.
//!
//! Positions arrive in root pixels; the event loop translates them to the target window
//! and its scale. A list larger than one property transfer (`INCR`) is not read.

use {
    std::{
        mem,
        os::raw::{c_int, c_long, c_uchar, c_ulong, c_void},
        ptr,
        sync::Arc,
    },
    self::super::x11_sys,
    crate::event::{DragItem, DragResponse},
};

pub struct Dnd {
    pub atoms: DndAtoms,
    pub display: *mut x11_sys::Display,
    session: Option<Session>,
}

/// One drag over one of our windows, from `XdndEnter` to `XdndLeave` or `XdndFinished`.
struct Session {
    source: x11_sys::Window,
    target: x11_sys::Window,
    version: c_long,
    /// The source offers `text/uri-list`.
    files: bool,
    /// The list was requested from the source.
    requested: bool,
    /// The dragged files, once the source answered.
    items: Option<Arc<Vec<DragItem>>>,
    /// The last `XdndPosition`, in root pixels.
    root: Option<(c_int, c_int)>,
    /// `XdndDrop` arrived before the list.
    dropped: bool,
    /// What the application last answered.
    response: DragResponse,
}

/// What the event loop tells the application after a protocol message.
pub enum DndStep {
    None,
    /// The drag is at `root` over `window`: answer with [`Dnd::send_status`].
    Drag { window: x11_sys::Window, root: (c_int, c_int), items: Arc<Vec<DragItem>> },
    /// The files were dropped at `root` over `window`: answer with [`Dnd::finish`].
    Drop { window: x11_sys::Window, root: (c_int, c_int), items: Arc<Vec<DragItem>> },
    /// The drag left the window or ended without a drop.
    End,
}

impl Dnd {
    pub unsafe fn new(display: *mut x11_sys::Display) -> Dnd {
        Dnd {
            atoms: DndAtoms::new(display),
            display,
            session: None,
        }
    }

    /// Enables drag-and-drop for the given window.
    pub unsafe fn enable_for_window(&mut self, window: x11_sys::Window) {
        let version = 5 as c_ulong;
        x11_sys::XChangeProperty(
            self.display,
            window,
            self.atoms.aware,
            4, // XA_ATOM
            32,
            x11_sys::PropModeReplace as c_int,
            &version as *const c_ulong as *const c_uchar,
            1
        );
    }

    pub unsafe fn handle_enter_event(&mut self, event: &x11_sys::XClientMessageEvent) -> DndStep {
        let source = event.data.l[0] as x11_sys::Window;
        let has_more_types = event.data.l[1] & 1 != 0;
        // Up to three types travel in the message itself; more are a property of the source.
        let types: Vec<x11_sys::Atom> = if has_more_types {
            self.get_type_list_property(source)
        } else {
            event.data.l[2..5].iter().map(|&l| l as x11_sys::Atom).collect()
        };
        self.session = Some(Session {
            source,
            target: event.window,
            version: (event.data.l[1] >> 24) & 0xff,
            files: types.contains(&self.atoms.uri_list),
            requested: false,
            items: None,
            root: None,
            dropped: false,
            response: DragResponse::None,
        });
        DndStep::None
    }

    pub unsafe fn handle_position_event(&mut self, event: &x11_sys::XClientMessageEvent) -> DndStep {
        let source = event.data.l[0] as x11_sys::Window;
        let uri_list = self.atoms.uri_list;
        let selection = self.atoms.selection;
        let display = self.display;
        let Some(session) = self.session.as_mut().filter(|session| session.source == source) else {
            self.send_status_to(source, event.window, DragResponse::None);
            return DndStep::None;
        };
        let packed = event.data.l[2];
        let root = (((packed >> 16) & 0xffff) as i16 as c_int, (packed & 0xffff) as i16 as c_int);
        session.root = Some(root);
        if !session.files {
            session.response = DragResponse::None;
            self.send_status_to(source, event.window, DragResponse::None);
            return DndStep::None;
        }
        if let Some(items) = &session.items {
            return DndStep::Drag { window: session.target, root, items: items.clone() };
        }
        if !session.requested {
            session.requested = true;
            // The position carries its timestamp from protocol version 1 on.
            let time = if session.version >= 1 { event.data.l[3] as x11_sys::Time } else { x11_sys::CurrentTime as x11_sys::Time };
            x11_sys::XConvertSelection(display, selection, uri_list, selection, session.target, time);
        }
        // The list is on its way; the widgets answer as soon as it is known.
        session.response = DragResponse::Copy;
        self.send_status_to(source, event.window, DragResponse::Copy);
        DndStep::None
    }

    /// The source answered (or refused, `property` None) the request for the file list.
    pub unsafe fn handle_selection_event(&mut self, event: &x11_sys::XSelectionEvent) -> DndStep {
        let data = if event.property == 0 { Vec::new() } else { self.take_property(event.requestor, event.property) };
        let Some(session) = self.session.as_mut() else { return DndStep::None };
        let items: Vec<DragItem> = file_paths_from_uri_list(&data)
            .into_iter()
            .map(|path| DragItem::FilePath { path, internal_id: None })
            .collect();
        let items = Arc::new(items);
        session.items = Some(items.clone());
        let Some(root) = session.root else { return DndStep::None };
        if session.dropped {
            DndStep::Drop { window: session.target, root, items }
        } else {
            DndStep::Drag { window: session.target, root, items }
        }
    }

    pub unsafe fn handle_drop_event(&mut self, event: &x11_sys::XClientMessageEvent) -> DndStep {
        let source = event.data.l[0] as x11_sys::Window;
        let uri_list = self.atoms.uri_list;
        let selection = self.atoms.selection;
        let display = self.display;
        let Some(session) = self.session.as_mut().filter(|session| session.source == source) else {
            self.send_finished_to(source, event.window, DragResponse::None);
            return DndStep::None;
        };
        let Some(root) = session.root.filter(|_| session.files) else {
            self.finish(false);
            return DndStep::End;
        };
        if let Some(items) = &session.items {
            return DndStep::Drop { window: session.target, root, items: items.clone() };
        }
        session.dropped = true;
        if !session.requested {
            session.requested = true;
            let time = if session.version >= 1 { event.data.l[2] as x11_sys::Time } else { x11_sys::CurrentTime as x11_sys::Time };
            x11_sys::XConvertSelection(display, selection, uri_list, selection, session.target, time);
            x11_sys::XFlush(display);
        }
        DndStep::None
    }

    pub unsafe fn handle_leave_event(&mut self, event: &x11_sys::XClientMessageEvent) -> DndStep {
        let source = event.data.l[0] as x11_sys::Window;
        if self.session.as_ref().is_some_and(|session| session.source == source) {
            self.session = None;
            return DndStep::End;
        }
        DndStep::None
    }

    /// Answers the last `XdndPosition` with what the application decided.
    pub unsafe fn send_status(&mut self, response: DragResponse) {
        let Some(session) = self.session.as_mut() else { return };
        session.response = response;
        let (source, target) = (session.source, session.target);
        self.send_status_to(source, target, response);
    }

    /// Ends the drop: tells the source whether the application took the files.
    pub unsafe fn finish(&mut self, accepted: bool) {
        let Some(session) = self.session.take() else { return };
        let response = match (accepted, session.response) {
            (false, _) => DragResponse::None,
            (true, DragResponse::None) => DragResponse::Copy,
            (true, response) => response,
        };
        self.send_finished_to(session.source, session.target, response);
    }

    fn action(&self, response: DragResponse) -> x11_sys::Atom {
        match response {
            DragResponse::None => x11_sys::None as x11_sys::Atom,
            DragResponse::Copy => self.atoms.action_copy,
            DragResponse::Move => self.atoms.action_move,
            DragResponse::Link => self.atoms.action_link,
        }
    }

    unsafe fn send_status_to(&self, source: x11_sys::Window, target: x11_sys::Window, response: DragResponse) {
        let accepted = response != DragResponse::None;
        // Bit 0 accepts; bit 1 asks for every further position (no "quiet" rectangle).
        self.send_message(source, self.atoms.status, [
            target as c_long,
            if accepted { 0b11 } else { 0b10 },
            0,
            0,
            self.action(response) as c_long,
        ]);
    }

    unsafe fn send_finished_to(&self, source: x11_sys::Window, target: x11_sys::Window, response: DragResponse) {
        self.send_message(source, self.atoms.finished, [
            target as c_long,
            if response != DragResponse::None { 1 } else { 0 },
            self.action(response) as c_long,
            0,
            0,
        ]);
    }

    unsafe fn send_message(&self, to: x11_sys::Window, message_type: x11_sys::Atom, l: [c_long; 5]) {
        let mut data = mem::zeroed::<x11_sys::XClientMessageEvent__bindgen_ty_1>();
        data.l = l;
        x11_sys::XSendEvent(
            self.display,
            to,
            x11_sys::False as c_int,
            x11_sys::NoEventMask as c_long,
            &mut x11_sys::XClientMessageEvent {
                type_: x11_sys::ClientMessage as c_int,
                serial: 0,
                send_event: 0,
                display: self.display,
                window: to,
                message_type,
                format: 32,
                data,
            } as *mut x11_sys::XClientMessageEvent as *mut x11_sys::XEvent
        );
        x11_sys::XFlush(self.display);
    }

    /// Reads and deletes a property the source wrote on our window.
    unsafe fn take_property(&self, window: x11_sys::Window, property: x11_sys::Atom) -> Vec<u8> {
        let mut actual_type = 0;
        let mut actual_format = 0;
        let mut nitems = 0;
        let mut bytes_after = 0;
        let mut prop = ptr::null_mut();
        let status = x11_sys::XGetWindowProperty(
            self.display,
            window,
            property,
            0,
            0x1fff_ffff,
            x11_sys::True as c_int,
            x11_sys::AnyPropertyType as x11_sys::Atom,
            &mut actual_type,
            &mut actual_format,
            &mut nitems,
            &mut bytes_after,
            &mut prop,
        );
        if status != 0 || prop.is_null() {
            return Vec::new();
        }
        let data = if actual_format == 8 && actual_type != self.atoms.incr {
            std::slice::from_raw_parts(prop as *const u8, nitems as usize).to_vec()
        } else {
            crate::log!("XDND: unsupported file list transfer (type {}, format {})", actual_type, actual_format);
            Vec::new()
        };
        x11_sys::XFree(prop as *mut c_void);
        data
    }

    /// Gets the XDndTypeList property from the source window.
    unsafe fn get_type_list_property(&mut self, source_window: x11_sys::Window) -> Vec<x11_sys::Atom> {
        let mut type_list = Vec::new();
        let mut offset = 0;
        let length = 1024;
        let mut actual_type = 0;
        let mut actual_format = 0;
        let mut nitems = 0;
        let mut bytes_after = 0;
        let mut prop = ptr::null_mut();
        loop {
            let status = x11_sys::XGetWindowProperty(
                self.display,
                source_window,
                self.atoms.type_list,
                offset,
                length,
                x11_sys::False as c_int,
                4, // XA_ATOM,
                &mut actual_type,
                &mut actual_format,
                &mut nitems,
                &mut bytes_after,
                &mut prop,
            );
            if status != 0 || prop.is_null() {
                break;
            }
            if actual_format == 32 {
                type_list.extend_from_slice(std::slice::from_raw_parts(prop as *mut x11_sys::Atom, nitems as usize));
            }
            x11_sys::XFree(prop as *mut c_void);
            if bytes_after == 0 || actual_format != 32 {
                break;
            }
            offset += length;
        };
        type_list
    }
}

pub struct DndAtoms {
    pub action_copy: x11_sys::Atom,
    pub action_move: x11_sys::Atom,
    pub action_link: x11_sys::Atom,
    pub aware: x11_sys::Atom,
    pub drop: x11_sys::Atom,
    pub enter: x11_sys::Atom,
    pub finished: x11_sys::Atom,
    pub incr: x11_sys::Atom,
    pub leave: x11_sys::Atom,
    pub position: x11_sys::Atom,
    pub selection: x11_sys::Atom,
    pub status: x11_sys::Atom,
    pub type_list: x11_sys::Atom,
    pub uri_list: x11_sys::Atom,
}

impl DndAtoms {
    pub unsafe fn new(display: *mut x11_sys::Display) -> DndAtoms {
        let atom = |name: &str| x11_sys::XInternAtom(display, name.as_ptr() as *const _, 0);
        DndAtoms {
            action_copy: atom("XdndActionCopy\0"),
            action_move: atom("XdndActionMove\0"),
            action_link: atom("XdndActionLink\0"),
            aware: atom("XdndAware\0"),
            drop: atom("XdndDrop\0"),
            enter: atom("XdndEnter\0"),
            finished: atom("XdndFinished\0"),
            incr: atom("INCR\0"),
            leave: atom("XdndLeave\0"),
            position: atom("XdndPosition\0"),
            selection: atom("XdndSelection\0"),
            status: atom("XdndStatus\0"),
            type_list: atom("XdndTypeList\0"),
            uri_list: atom("text/uri-list\0"),
        }
    }
}

/// The local file paths of a `text/uri-list` (RFC 2483): one URI per line, lines end in
/// CRLF (some sources send bare LF or a trailing NUL), `#` lines are comments. Only `file:`
/// URIs name files: `file:///path`, `file://host/path` (the host is this machine: the drag
/// came through its display) and `file:/path`. Paths are percent-encoded UTF-8.
pub fn file_paths_from_uri_list(data: &[u8]) -> Vec<String> {
    data.split(|byte| matches!(byte, b'\n' | b'\r' | 0))
        .filter_map(|line| file_path_from_uri(line.trim_ascii()))
        .collect()
}

fn file_path_from_uri(uri: &[u8]) -> Option<String> {
    let scheme = uri.get(..5)?;
    if !scheme.eq_ignore_ascii_case(b"file:") {
        return None;
    }
    let rest = &uri[5..];
    let path = match rest.strip_prefix(b"//") {
        Some(authority) => &authority[authority.iter().position(|byte| *byte == b'/')?..],
        None => rest,
    };
    if path.first() != Some(&b'/') {
        return None;
    }
    let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    let mut decoded = Vec::with_capacity(path.len());
    let mut index = 0;
    while index < path.len() {
        let escaped = (path[index] == b'%')
            .then(|| Some((hex(*path.get(index + 1)?)?, hex(*path.get(index + 2)?)?)))
            .flatten();
        match escaped {
            Some((high, low)) => {
                decoded.push(high << 4 | low);
                index += 3;
            }
            None => {
                decoded.push(path[index]);
                index += 1;
            }
        }
    }
    Some(String::from_utf8(decoded).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::file_paths_from_uri_list;

    #[test]
    fn uri_list_lines_become_decoded_local_paths() {
        // What a file manager sends for an ASCII name, a Korean name with spaces and a
        // name with reserved characters: CRLF lines, percent-encoded UTF-8.
        let list = "file:///home/u/notes.txt\r\n\
            file:///home/u/%ED%95%9C%EA%B8%80%20%ED%8C%8C%EC%9D%BC%20%EC%9D%B4%EB%A6%84.txt\r\n\
            file:///home/u/a%23b%3Fc%25d+e.txt\r\n";
        assert_eq!(
            file_paths_from_uri_list(list.as_bytes()),
            ["/home/u/notes.txt", "/home/u/한글 파일 이름.txt", "/home/u/a#b?c%d+e.txt"]
        );
    }

    #[test]
    fn uri_list_accepts_every_local_file_form() {
        let list = "# dragged from another toolkit\n\
            file://localhost/tmp/a.txt\n\
            file://workstation/tmp/b.txt\n\
            file:/tmp/c.txt\n\
            FILE:///tmp/d.txt\n\
            \n\
            https://example.com/e.txt\n\
            file:relative.txt\n\
            file:///tmp/100%.txt\0";
        assert_eq!(
            file_paths_from_uri_list(list.as_bytes()),
            ["/tmp/a.txt", "/tmp/b.txt", "/tmp/c.txt", "/tmp/d.txt", "/tmp/100%.txt"]
        );
    }
}
