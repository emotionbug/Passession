#![allow(dead_code)]
#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
use {
    std::cell::{Cell, RefCell},
    crate::{
        event::DragItem,
        windows::{
            core::implement,
            core,
            Win32::{
                System::{
                    Ole::{
                        IDropTarget,
                        DROPEFFECT,
                        DROPEFFECT_NONE,
                        CF_HDROP,
                        IDropTarget_Impl,
                    },
                    Com::{
                        IDataObject,
                        FORMATETC,
                        DVASPECT_CONTENT,
                        TYMED_HGLOBAL,
                    },
                    SystemServices::MODIFIERKEYS_FLAGS,
                },
                Foundation::{
                    WPARAM,
                    LPARAM,
                    HWND,
                    POINTL,
                },
                UI::WindowsAndMessaging::{
                    WM_USER,
                    SendMessageW,
                },
            },
        },
        os::windows::{
            dropfiles::*,
        },
    },
};

// Paseo: upstream carried exactly one file (a multi-file drop was discarded in DragEnter,
// so nothing was ever delivered for it) and never wrote the drop effect back: the source
// kept showing "allowed" wherever the pointer was, and received its own allowed effects
// (possibly "move") as the result of the drop. Every message now carries all files, and
// the window procedure's result is the effect the widgets chose.

/// The `DROPEFFECT` in each message is what the drag source allows.
#[derive(Clone)]
pub enum DropTargetMessage {
    Enter(MODIFIERKEYS_FLAGS,POINTL,DROPEFFECT,Vec<DragItem>),
    Leave,
    Over(MODIFIERKEYS_FLAGS,POINTL,DROPEFFECT,Vec<DragItem>),
    Drop(MODIFIERKEYS_FLAGS,POINTL,DROPEFFECT,Vec<DragItem>),
}

// This uses WM_USER to send user messages back to the message queue of the window; careful when using WM_USER elsewhere
pub const WM_DROPTARGET: u32 = WM_USER + 0;

#[derive(Clone)]
#[implement(IDropTarget)]
pub struct DropTarget {
    pub drag_items: RefCell<Vec<DragItem>>,  // Windows only provides the data for Enter and Drop, but makepad needs it for Over as well
    pub effect: Cell<DROPEFFECT>,  // what the widgets last answered; a drop they take reports it
    pub hwnd: HWND,  // which window to send the messages to
}

/// The files of a drag (`CF_HDROP`); empty when it carries none.
fn drag_items_from_data_object(data_object: &IDataObject) -> Vec<DragItem> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    match unsafe { data_object.GetData(&format) } {
        Ok(medium) => convert_medium_to_dragitems(medium),
        Err(_) => Vec::new(),
    }
}

impl DropTarget {
    /// Hands the message to the window and returns its answer: the drop effect the
    /// widgets chose for Enter/Over, nonzero for a Drop a widget took.
    fn send(&self, message: DropTargetMessage) -> isize {
        let param = Box::new(message);
        let result = unsafe { SendMessageW(
            self.hwnd,
            WM_DROPTARGET,
            WPARAM(0),
            LPARAM(Box::into_raw(param) as isize)
        ) };
        result.0
    }

    /// Remembers the effect and reports it to the drag source.
    fn answer(&self, pdweffect: *mut DROPEFFECT, effect: DROPEFFECT) {
        self.effect.set(effect);
        if !pdweffect.is_null() {
            unsafe { *pdweffect = effect };
        }
    }

    fn allowed(pdweffect: *mut DROPEFFECT) -> DROPEFFECT {
        if pdweffect.is_null() { DROPEFFECT_NONE } else { unsafe { *pdweffect } }
    }
}

// IDropTarget implementation for DropTarget, which sends WM_DROPTARGET messages to the window as they appear

impl IDropTarget_Impl for DropTarget {

    fn DragEnter(&self,_p_data_obj: Option<&IDataObject>,_grf_key_state: MODIFIERKEYS_FLAGS,_pt: &POINTL,_pdweffect: *mut DROPEFFECT) -> core::Result<()> {

        let items = _p_data_obj.map(drag_items_from_data_object).unwrap_or_default();

        // store locally for Over messages
        self.drag_items.replace(items.clone());

        // nothing we can take: refuse, so the source shows its "no drop" cursor
        if items.is_empty() {
            self.answer(_pdweffect, DROPEFFECT_NONE);
            return Ok(());
        }

        let effect = self.send(DropTargetMessage::Enter(_grf_key_state,*_pt,Self::allowed(_pdweffect),items));
        self.answer(_pdweffect, DROPEFFECT(effect as u32));

        Ok(())
    }

    fn DragLeave(&self) -> core::Result<()> {

        // forget the locally stored data
        self.drag_items.replace(Vec::new());

        self.send(DropTargetMessage::Leave);

        Ok(())
    }

    fn DragOver(&self,_grf_key_state: MODIFIERKEYS_FLAGS,_pt: &POINTL,_pdweffect: *mut DROPEFFECT) -> core::Result<()> {

        let items = self.drag_items.borrow().clone();
        if items.is_empty() {
            self.answer(_pdweffect, DROPEFFECT_NONE);
            return Ok(());
        }

        let effect = self.send(DropTargetMessage::Over(_grf_key_state,*_pt,Self::allowed(_pdweffect),items));
        self.answer(_pdweffect, DROPEFFECT(effect as u32));

        Ok(())
    }

    fn Drop(&self,_p_data_obj: Option<&IDataObject>, _grf_key_state: MODIFIERKEYS_FLAGS, _pt: &POINTL,_pdweffect: *mut DROPEFFECT) -> core::Result<()> {

        let items = _p_data_obj.map(drag_items_from_data_object).unwrap_or_default();

        // forget the locally stored data, after Drop we don't need it anymore
        self.drag_items.replace(Vec::new());

        if items.is_empty() {
            self.answer(_pdweffect, DROPEFFECT_NONE);
            self.send(DropTargetMessage::Leave);
            return Ok(());
        }

        let effect = self.effect.get();
        let taken = self.send(DropTargetMessage::Drop(_grf_key_state,*_pt,Self::allowed(_pdweffect),items)) != 0;
        self.answer(_pdweffect, if taken { effect } else { DROPEFFECT_NONE });

        Ok(())
    }
}
