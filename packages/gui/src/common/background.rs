//! Work that must not run inside a GUI event: parsing, highlighting, decoding,
//! file reads. Native builds give each job its own named thread. The browser has
//! no threads: jobs queue and the app runs them between events, a few
//! milliseconds at a time ([`run_queued`]), so input and painting keep going.
//!
//! A job delivers its result the way the native thread always did (a channel or
//! `Cx::post_action`) and wakes the GUI with `SignalToUI::set_ui_signal`.

use std::io;

/// Runs `job` away from the current GUI event; errors only when the job could not
/// be started (then it never runs).
pub fn spawn(name: &str, job: impl FnOnce() + Send + 'static) -> io::Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(job)
            .map(drop)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = name;
        browser::push(Box::new(job));
        Ok(())
    }
}

/// Runs queued jobs for about `budget`; true when some remain. A job is never
/// interrupted, and the first always runs.
#[cfg(target_arch = "wasm32")]
pub fn run_queued(budget: std::time::Duration) -> bool {
    browser::run(budget)
}

#[cfg(target_arch = "wasm32")]
mod browser {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    type Job = Box<dyn FnOnce()>;

    thread_local! {
        static QUEUE: RefCell<VecDeque<Job>> = const { RefCell::new(VecDeque::new()) };
    }

    pub(super) fn push(job: Job) {
        QUEUE.with(|queue| queue.borrow_mut().push_back(job));
        makepad_widgets::SignalToUI::set_ui_signal();
    }

    pub(super) fn run(budget: Duration) -> bool {
        let started = Instant::now();
        loop {
            // Jobs may queue more jobs: never hold the borrow while one runs.
            let Some(job) = QUEUE.with(|queue| queue.borrow_mut().pop_front()) else {
                return false;
            };
            job();
            if started.elapsed() >= budget {
                return QUEUE.with(|queue| !queue.borrow().is_empty());
            }
        }
    }
}
