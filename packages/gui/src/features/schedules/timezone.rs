//! Original `utils/device-timezone.ts` as ported in Flutter `schedule_form_sheet.dart`:
//! the device's IANA zone from `TZ`, Android's `persist.sys.timezone` or the
//! `/etc/localtime` zoneinfo link (`/etc/timezone` on Linux), else the fixed-offset
//! `Etc/GMT∓N` zone for whole-hour offsets, else UTC. Probing runs once on a background
//! thread (it reads the filesystem or spawns `getprop`), never on the GUI thread.

use std::sync::{Mutex, OnceLock};

use chrono::{Local, Offset};

static PROBED: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn iana(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches(':');
    let mut segments = value.split('/');
    let first = segments.next()?;
    let valid_first = !first.is_empty() && first.chars().all(|c| c.is_ascii_alphabetic());
    let valid_rest = segments.all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')));
    (valid_first && valid_rest).then(|| value.to_owned())
}

fn probe() -> Option<String> {
    if let Some(zone) = std::env::var("TZ").ok().as_deref().and_then(iana) {
        return Some(zone);
    }
    if cfg!(target_os = "android") && let Ok(output) = std::process::Command::new("getprop").arg("persist.sys.timezone").output() && let Some(zone) = iana(&String::from_utf8_lossy(&output.stdout)) {
        return Some(zone);
    }
    if cfg!(any(target_os = "linux", target_os = "macos")) && let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        if let Some(index) = target.find("zoneinfo/") && let Some(zone) = iana(&target[index + "zoneinfo/".len()..]) {
            return Some(zone);
        }
    }
    if cfg!(target_os = "linux") && let Ok(text) = std::fs::read_to_string("/etc/timezone") {
        return iana(&text);
    }
    None
}

/// Fallback when the platform exposes no IANA zone.
pub fn offset_time_zone() -> String {
    let seconds = Local::now().offset().fix().local_minus_utc();
    if seconds == 0 || seconds % 3600 != 0 {
        return "UTC".into();
    }
    let hours = seconds / 3600;
    if hours > 0 { format!("Etc/GMT-{hours}") } else { format!("Etc/GMT+{}", -hours) }
}

/// The probed zone once known; the offset zone until then. The first call starts the probe.
pub fn device_time_zone() -> String {
    let cell = PROBED.get_or_init(|| {
        std::thread::spawn(|| {
            let zone = probe().unwrap_or_else(offset_time_zone);
            if let Some(cell) = PROBED.get() {
                *cell.lock().unwrap_or_else(|p| p.into_inner()) = Some(zone);
            }
            makepad_widgets::SignalToUI::set_ui_signal();
        });
        Mutex::new(None)
    });
    cell.lock().unwrap_or_else(|p| p.into_inner()).clone().unwrap_or_else(offset_time_zone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iana_shapes() {
        assert_eq!(iana(":Europe/Berlin\n").as_deref(), Some("Europe/Berlin"));
        assert_eq!(iana("America/Argentina/Buenos_Aires").as_deref(), Some("America/Argentina/Buenos_Aires"));
        assert!(iana("../../etc").is_none());
        assert!(iana("").is_none());
    }
}
