//! macOS and Windows status-bar / notification-area icon (original tray_manager):
//! tooltip "Paseo", menu Open Paseo / separator / Quit Paseo, a left click or
//! double click opens the preferred window. Created on the GUI (main) thread,
//! whose run loop / message loop services it.

use super::{deliver, NativeSignal};
use crate::platform::Shared;
use std::sync::{atomic::Ordering, Arc};

pub(crate) struct Tray {
    _icon: tray_icon::TrayIcon,
}

impl Tray {
    pub fn poll(&mut self) {}
}

pub(crate) fn create_tray(shared: &Arc<Shared>) {
    let owner = shared.clone();
    shared.on_main(move |_, state| match build_tray() {
        Ok(tray) => {
            if !owner.desktop.quitting() {
                state.desktop.tray = Some(tray);
                owner.desktop.tray_created.store(true, Ordering::SeqCst);
            }
        }
        Err(message) => owner.desktop.report(&message),
    });
}

fn build_tray() -> Result<Tray, String> {
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
    };
    // macOS draws the menu-bar icon as a template (alpha only, recoloured).
    #[cfg(target_os = "macos")]
    let bytes = &include_bytes!("../../assets/trayTemplate.png")[..];
    #[cfg(not(target_os = "macos"))]
    let bytes = &include_bytes!("../../assets/tray.png")[..];
    let (width, height, rgba) = crate::image::decode_png_rgba(bytes).map_err(|e| format!("Unable to load tray icon: {e}"))?;
    let icon = tray_icon::Icon::from_rgba(rgba, width, height).map_err(|e| format!("Unable to load tray icon: {e}"))?;
    let menu = Menu::new();
    let open = MenuItem::new("Open Paseo", true, None);
    let quit = MenuItem::new("Quit Paseo", true, None);
    menu.append(&open).map_err(|e| e.to_string())?;
    menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
    menu.append(&quit).map_err(|e| e.to_string())?;
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            deliver(NativeSignal::TrayOpen);
        } else if event.id == quit_id {
            deliver(NativeSignal::TrayQuit);
        }
    }));
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| match event {
        TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
        | TrayIconEvent::DoubleClick { .. } => deliver(NativeSignal::TrayOpen),
        _ => {}
    }));
    let builder = TrayIconBuilder::new().with_id(crate::paths::DESKTOP_APP_ID).with_tooltip("Paseo");
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(icon);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(icon);
    let icon = builder
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| format!("Unable to create the system tray icon: {e}"))?;
    Ok(Tray { _icon: icon })
}

/// macOS and Windows always host status/notification-area icons.
pub(crate) async fn tray_host_available() -> bool {
    true
}
