use crate::{
    log,
    live_id::LiveId,
    event::DragItem,
    windows::Win32::{
        System::{
            Com::{
                STGMEDIUM,
                TYMED_HGLOBAL,
            },
            Ole::ReleaseStgMedium,
            Memory::{
                GlobalAlloc,
                GlobalLock,
                GlobalUnlock,
                GlobalSize,
                GMEM_ZEROINIT,
                GMEM_FIXED,
            },
        },
        UI::Shell::{
            DragQueryFileW,
            HDROP,
        },
        Foundation::HGLOBAL,
    },
};

// This is where all binary conversion code goes between makepad DragItem and Windows HGLOBAL/DROPFILES structure

// Paseo: convert an incoming CF_HDROP medium from an internal or external source to one
// DragItem per file. Upstream decoded the DROPFILES block by hand, one UTF-16 unit per
// character (names outside the BMP came out wrong), refused the ANSI form, and returned
// nothing at all when the user dragged more than one file. The shell reads every form.
pub fn convert_medium_to_dragitems(medium: STGMEDIUM) -> Vec<DragItem> {

    let mut items = Vec::new();

    if medium.tymed == TYMED_HGLOBAL.0 as u32 {
        let hglobal = unsafe { medium.u.hGlobal };

        // an internal drag carries its LiveId between the DROPFILES header and the names
        let mut internal_id: Option<LiveId> = None;
        let hglobal_size = unsafe { GlobalSize(hglobal) };
        let hglobal_raw_ptr = unsafe { GlobalLock(hglobal) };
        if !hglobal_raw_ptr.is_null() {
            if hglobal_size >= 28 {
                let u32_slice = unsafe { std::slice::from_raw_parts(hglobal_raw_ptr as *const u32,7) };
                let id = ((u32_slice[6] as u64)<<32)|(u32_slice[5] as u64);
                if u32_slice[0] == 28 && id != 0 {
                    internal_id = Some(LiveId(id));
                }
            }
            let _ = unsafe { GlobalUnlock(hglobal) };
        }

        let hdrop = HDROP(hglobal.0 as isize);
        let count = unsafe { DragQueryFileW(hdrop,0xFFFF_FFFF,None) };
        for index in 0..count {
            let length = unsafe { DragQueryFileW(hdrop,index,None) } as usize;
            let mut name = vec![0u16;length + 1];
            let written = unsafe { DragQueryFileW(hdrop,index,Some(&mut name)) } as usize;
            if written == 0 {
                continue;
            }
            items.push(DragItem::FilePath { path: String::from_utf16_lossy(&name[..written.min(length)]),internal_id, });
        }
    }

    unsafe { ReleaseStgMedium(&medium as *const STGMEDIUM as *mut STGMEDIUM) };

    items
}

// create new internal DROPFILES structure from DragItem
pub fn create_hglobal_for_dragitem(drag_item: &DragItem) -> Option<HGLOBAL> {

    if let DragItem::FilePath { path,internal_id, } = drag_item {

        // encode filename
        let mut encoded_filename: Vec<u16> = path.encode_utf16().collect();
        encoded_filename.push(0);

        // only one filename
        encoded_filename.push(0);

        // create HGLOBAL to contain DROPFILES structure, the internal ID and this encoded filename
        let size_in_bytes = 28 + encoded_filename.len() * 2;
        let hglobal = unsafe { GlobalAlloc(GMEM_ZEROINIT | GMEM_FIXED,size_in_bytes) }.unwrap();
        let hglobal_raw_ptr = unsafe { GlobalLock(hglobal) };

        // initialize DROPFILES part
        let u32_slice = unsafe { std::slice::from_raw_parts_mut(hglobal_raw_ptr as *mut u32,7) };
        u32_slice[0] = 28; // offset to filename
        u32_slice[1] = 0;
        u32_slice[2] = 0;
        u32_slice[3] = 0;
        u32_slice[4] = 1; // not 0 because 16-bit characters in the filename

        // initialize internal ID
        if let Some(internal_id) = internal_id {
            u32_slice[6] = (internal_id.0>>32) as u32;
            u32_slice[5] = (internal_id.0&0xffff_ffff) as u32 ;
            //let u64_slice = unsafe { std::slice::from_raw_parts_mut((hglobal_raw_ptr as *mut u8).offset(20) as *mut u64,1) };
            //u64_slice[0] = internal_id.0;
        }

        // initialize filename
        unsafe {
            std::ptr::copy_nonoverlapping(
                encoded_filename.as_ptr(),
                (hglobal_raw_ptr as *mut u8).offset(28) as *mut u16,
                encoded_filename.len(),
            )
        };

        // ready
        unsafe { GlobalUnlock(hglobal) }.unwrap();

        Some(hglobal)
    }

    else {
        log!("only DragItem::FilePath supported");
        None
    }
}