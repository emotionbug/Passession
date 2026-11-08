//! Keeps a `!Send` value on one dedicated thread and runs calls there.

use std::sync::mpsc;
use std::thread::JoinHandle;

type Job<T> = Box<dyn FnOnce(&mut T) + Send>;

/// Owns a value created on, and never moved off, its own named thread.
/// Calls are serialized in submission order and block the caller until done.
pub struct ThreadBound<T: 'static> {
    jobs: Option<mpsc::Sender<Job<T>>>,
    thread: Option<JoinHandle<()>>,
}

impl<T: 'static> ThreadBound<T> {
    /// Starts the thread and builds the value there.
    pub fn spawn<E: Send + 'static>(name: &str, init: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<Self, E> {
        let (jobs, receiver) = mpsc::channel::<Job<T>>();
        let (ready, started) = mpsc::sync_channel::<Result<(), E>>(1);
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut value = match init() {
                    Ok(value) => {
                        let _ = ready.send(Ok(()));
                        value
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while let Ok(job) = receiver.recv() {
                    job(&mut value);
                }
            })
            .expect("spawn terminal emulator thread");
        started.recv().expect("terminal emulator thread reports startup")?;
        Ok(Self { jobs: Some(jobs), thread: Some(thread) })
    }

    /// Runs `f` on the owning thread and returns its result.
    ///
    /// # Panics
    /// Re-raises a panic from an earlier or the current call: the value is
    /// gone with its thread.
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut T) -> R + Send + 'static) -> R {
        let (reply, result) = mpsc::sync_channel(1);
        self.jobs
            .as_ref()
            .expect("jobs open until drop")
            .send(Box::new(move |value: &mut T| {
                let _ = reply.send(f(value));
            }))
            .expect("terminal emulator thread stopped");
        result.recv().expect("terminal emulator thread stopped")
    }
}

impl<T: 'static> Drop for ThreadBound<T> {
    fn drop(&mut self) {
        // Closing the queue ends the thread after queued calls; the value is
        // dropped there, on the thread that created it.
        self.jobs.take();
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ThreadBound;
    use std::rc::Rc;

    #[test]
    fn non_send_values_stay_on_their_thread() {
        let bound = ThreadBound::spawn("bound-test", || Ok::<_, ()>(Rc::new(std::thread::current().id()))).unwrap();
        let owner = bound.call(|value| **value);
        assert_ne!(owner, std::thread::current().id());
        assert_eq!(bound.call(|_| std::thread::current().id()), owner);
    }

    #[test]
    fn startup_errors_are_returned() {
        assert_eq!(ThreadBound::<Rc<u8>>::spawn("bound-fail", || Err("no")).err(), Some("no"));
    }
}
