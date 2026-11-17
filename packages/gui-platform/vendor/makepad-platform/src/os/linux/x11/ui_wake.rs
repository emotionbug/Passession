use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    sync::LazyLock,
};

// Linux eventfd flags (O_NONBLOCK and O_CLOEXEC). No Xlib calls are made by posters.
const EFD_NONBLOCK: i32 = 0x800;
const EFD_CLOEXEC: i32 = 0x80000;

extern "C" {
    fn eventfd(initval: u32, flags: i32) -> i32;
}

pub(crate) struct UiWake(File);

// The Xlib app itself is process-global. Retain the fd for the same lifetime so
// a worker posting during shutdown can never write to a closed/reused descriptor.
static UI_WAKE: LazyLock<UiWake> = LazyLock::new(UiWake::new);

pub(crate) fn ui_wake() -> &'static UiWake {
    &UI_WAKE
}

pub(crate) fn wake_ui() {
    ui_wake().notify();
}

impl UiWake {
    fn new() -> Self {
        let fd = unsafe { eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC) };
        assert!(fd >= 0, "creating X11 UI wake eventfd: {}", io::Error::last_os_error());
        Self(unsafe { File::from_raw_fd(fd) })
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    fn notify(&self) {
        loop {
            match (&self.0).write(&1u64.to_ne_bytes()) {
                Ok(8) => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // A saturated counter is already readable; no wake can be lost.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                result => panic!("posting X11 UI wake: {result:?}"),
            }
        }
    }

    pub(crate) fn drain(&self) -> bool {
        let mut counter = [0; 8];
        loop {
            match (&self.0).read(&mut counter) {
                // One eventfd read consumes every coalesced post, not the flags or
                // channel contents. Posts after this read remain readable.
                Ok(8) => return true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
                result => panic!("draining X11 UI wake: {result:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{os::linux::select_timer::SelectTimers, thread::SignalToUI};
    use std::os::unix::net::UnixStream;

    #[test]
    fn ui_wake_coalesces_posts_and_rearms_after_drain() {
        let wake = UiWake::new();
        assert!(!wake.drain());
        for _ in 0..1000 {
            wake.notify();
        }
        assert!(wake.drain());
        assert!(!wake.drain());
        wake.notify();
        assert!(wake.drain());
        assert!(!wake.drain());
    }

    #[test]
    fn background_signals_and_actions_wake_select_without_x_events() {
        let wake = ui_wake();
        wake.drain();
        SignalToUI::check_and_clear_ui_signal();
        SignalToUI::check_and_clear_action_signal();
        let (x_connection, _peer) = UnixStream::pair().unwrap();
        let signal = SignalToUI::new();
        for action in [false, true] {
            let posted_signal = signal.clone();
            let poster = std::thread::spawn(move || {
                if action {
                    SignalToUI::set_action_signal();
                } else {
                    posted_signal.set();
                }
            });
            // A deadline prevents a broken wake from hanging the test suite.
            let mut timers = SelectTimers::new();
            timers.start_timer(1, 1.0, false);
            timers.select(&[x_connection.as_raw_fd(), wake.fd()]);
            assert!(wake.drain());
            poster.join().unwrap();
            if action {
                assert!(SignalToUI::check_and_clear_action_signal());
            } else {
                assert!(SignalToUI::check_and_clear_ui_signal());
                assert!(signal.check_and_clear());
            }
            assert!(!wake.drain());
        }
    }

    #[test]
    fn timer_deadline_wakes_select_without_a_signal() {
        let wake = UiWake::new();
        let mut timers = SelectTimers::new();
        timers.start_timer(7, 0.001, false);
        timers.select(&[wake.fd()]);
        let mut fired = Vec::new();
        timers.update_timers(&mut fired);
        assert_eq!(fired, vec![7]);
        assert!(!wake.drain());
    }
}
