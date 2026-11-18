//! File URLs at the native drag boundary; DragItem paths are filesystem paths.

use crate::{
    event::DragItem,
    makepad_live_id::LiveId,
    os::apple::{
        apple_sys::*,
        apple_util::{nsstring_to_string, str_to_nsstring},
    },
};

pub(super) unsafe fn from_url(url: ObjcId) -> Option<DragItem> {
    if url == nil {
        return None;
    }
    let file: BOOL = msg_send![url, isFileURL];
    if file != YES {
        return None;
    }
    let fragment: ObjcId = msg_send![url, fragment];
    let internal_id = if fragment == nil {
        None
    } else {
        nsstring_to_string(fragment)
            .strip_prefix("makepad_internal_id=")
            .and_then(|value| value.parse::<u64>().ok())
            .map(LiveId)
    };
    let url: ObjcId = msg_send![url, filePathURL];
    if url == nil {
        return None;
    }
    // `absoluteString` is percent-encoded. NSURL's path decodes it once and
    // keeps literal percent sequences and '#' characters in the filename.
    let path: ObjcId = msg_send![url, path];
    if path == nil {
        return None;
    }
    Some(DragItem::FilePath {
        path: nsstring_to_string(path),
        internal_id,
    })
}

pub(super) unsafe fn to_url(path: &str, internal_id: Option<LiveId>) -> ObjcId {
    let url: ObjcId = if path.is_empty() {
        msg_send![class!(NSURL), URLWithString: str_to_nsstring("file://makepad_internal_empty")]
    } else {
        msg_send![class!(NSURL), fileURLWithPath: str_to_nsstring(path)]
    };
    let Some(id) = internal_id else {
        return url;
    };
    let components: ObjcId =
        msg_send![class!(NSURLComponents), componentsWithURL: url resolvingAgainstBaseURL: NO];
    let _: () = msg_send![components, setFragment: str_to_nsstring(&format!("makepad_internal_id={}", id.0))];
    msg_send![components, URL]
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn canonical(text: &str) -> String {
        let string: ObjcId =
            msg_send![str_to_nsstring(text), precomposedStringWithCanonicalMapping];
        nsstring_to_string(string)
    }

    #[test]
    fn file_drop_paths_are_decoded_once_and_readable() {
        unsafe {
            let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("paseo-file-drop-{}-{unique}", std::process::id()));
            std::fs::create_dir(&root).unwrap();
            for name in [
                "한글 첨부 파일.txt",
                "한글.txt",
                "emoji 😀.txt",
                "literal%ED%95%9C.txt",
                "100% #makepad_internal_id=42.txt",
            ] {
                let path = root.join(name);
                std::fs::write(&path, b"attachment bytes").unwrap();
                // Finder supplies NSURLs, including their encoded URL representation.
                let file: ObjcId = msg_send![class!(NSURL), fileURLWithPath: str_to_nsstring(path.to_str().unwrap())];
                let encoded: ObjcId = msg_send![file, absoluteString];
                let dragged: ObjcId = msg_send![class!(NSURL), URLWithString: encoded];
                let Some(DragItem::FilePath {
                    path: dropped,
                    internal_id,
                }) = from_url(dragged)
                else {
                    panic!("file URL was rejected");
                };
                assert_eq!(canonical(&dropped), canonical(path.to_str().unwrap()));
                assert_eq!(internal_id, None);
                assert_eq!(std::fs::read(&dropped).unwrap(), b"attachment bytes");
            }
            std::fs::remove_dir_all(root).unwrap();
            let _: () = msg_send![pool, drain];
        }
    }

    #[test]
    fn file_drag_urls_preserve_names_and_internal_ids() {
        unsafe {
            let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
            for path in [
                "/tmp/한글 첨부 % #.txt",
                "/tmp/literal%20.txt",
                "/tmp/#makepad_internal_id=99",
                "",
            ] {
                for id in [None, Some(LiveId(42))] {
                    let url = to_url(path, id);
                    let Some(DragItem::FilePath {
                        path: dropped,
                        internal_id,
                    }) = from_url(url)
                    else {
                        panic!("file URL was rejected");
                    };
                    assert_eq!(canonical(&dropped), canonical(path));
                    assert_eq!(internal_id, id);
                }
            }
            let _: () = msg_send![pool, drain];
        }
    }

    #[test]
    fn file_drop_rejects_non_file_urls() {
        unsafe {
            let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
            let url: ObjcId = msg_send![class!(NSURL), URLWithString: str_to_nsstring("https://example.com/한글.txt")];
            assert!(from_url(url).is_none());
            assert!(from_url(nil).is_none());
            let _: () = msg_send![pool, drain];
        }
    }
}
