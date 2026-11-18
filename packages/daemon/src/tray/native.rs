use super::{CONFIRM_LABEL, Confirmation, OPEN_LABEL, STOP_LABEL, icon, open_app, request_stop};
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use std::path::Path;
use tokio_util::sync::CancellationToken;
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem},
};

#[cfg(target_os = "macos")]
pub(super) fn interactive_session() -> bool {
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SessionGetInfo(session: u32, id: *mut u32, attributes: *mut u32) -> i32;
    }

    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
    let mut attributes = 0;
    // A service's login context cannot be changed by a GUI login elsewhere.
    let found = unsafe {
        SessionGetInfo(
            CALLER_SECURITY_SESSION,
            std::ptr::null_mut(),
            &mut attributes,
        ) == 0
    };
    found && attributes & SESSION_HAS_GRAPHIC_ACCESS != 0
}

#[cfg(windows)]
pub(super) fn interactive_session() -> bool {
    use windows_sys::Win32::{
        System::{
            RemoteDesktop::ProcessIdToSessionId,
            StationsAndDesktops::{
                GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
            },
        },
        UI::WindowsAndMessaging::WSF_VISIBLE,
    };

    let mut session = 0;
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut session) } == 0 || session == 0 {
        return false;
    }
    let station = unsafe { GetProcessWindowStation() };
    let mut flags = USEROBJECTFLAGS::default();
    let visible = unsafe {
        !station.is_null()
            && GetUserObjectInformationW(
                station,
                UOI_FLAGS,
                (&mut flags as *mut USEROBJECTFLAGS).cast(),
                std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                &mut 0,
            ) != 0
    };
    visible && flags.dwFlags & WSF_VISIBLE as u32 != 0
}

#[cfg(target_os = "macos")]
pub(super) fn run(home: &Path, title: &str, cancel: CancellationToken) -> Result<()> {
    use objc2::{MainThreadMarker, rc::autoreleasepool};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode};

    autoreleasepool(|_| {
        let main = MainThreadMarker::new().context("Daemon tray requires the main thread")?;
        let app = NSApplication::sharedApplication(main);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        event_loop(home, title, cancel, || {
            autoreleasepool(|_| {
                let expiration = NSDate::dateWithTimeIntervalSinceNow(0.2);
                if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&expiration),
                    unsafe { NSDefaultRunLoopMode },
                    true,
                ) {
                    app.sendEvent(&event);
                }
            });
            Ok(true)
        })
    })
}

#[cfg(windows)]
pub(super) fn run(home: &Path, title: &str, cancel: CancellationToken) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::WAIT_FAILED,
        UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
            PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
        },
    };

    event_loop(home, title, cancel, || unsafe {
        if MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 200, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            == WAIT_FAILED
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut message = MSG::default();
        // Limit each batch so a busy message queue cannot starve cancellation.
        for _ in 0..64 {
            if PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) == 0 {
                break;
            }
            if message.message == WM_QUIT {
                return Ok(false);
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        Ok(true)
    })
}

fn event_loop(
    home: &Path,
    title: &str,
    cancel: CancellationToken,
    mut pump: impl FnMut() -> Result<bool>,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (width, height, rgba) = icon()?;
    let image = Icon::from_rgba(rgba, width, height)?;
    let menu = Menu::new();
    let status = MenuItem::new(title.replace('&', "&&"), false, None);
    let open = MenuItem::new(OPEN_LABEL, true, None);
    let stop = MenuItem::new(STOP_LABEL, true, None);
    let confirm = MenuItem::new(CONFIRM_LABEL, true, None);
    menu.append_items(&[&status, &open, &stop])?;
    let builder = TrayIconBuilder::new()
        .with_tooltip(title)
        .with_menu(Box::new(menu.clone()));
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(image);
    #[cfg(windows)]
    let builder = builder.with_icon(image);
    let _tray = builder.build()?;
    let mut confirmation = Confirmation::default();
    while !cancel.is_cancelled() && pump()? {
        if confirmation.expire() {
            menu.remove(&confirm)?;
            menu.append(&stop)?;
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if &event.id == open.id() {
                runtime.block_on(open_app(home));
            } else if &event.id == stop.id() && confirmation.0.is_none() {
                confirmation.arm();
                menu.remove(&stop)?;
                menu.append(&confirm)?;
            } else if &event.id == confirm.id() && confirmation.confirmed() {
                request_stop()?;
                return Ok(());
            }
        }
    }
    Ok(())
}
