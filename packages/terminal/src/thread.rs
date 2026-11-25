//! Keeps a `!Send` value on its owning thread and runs calls there.

#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;

#[cfg(not(target_arch = "wasm32"))]
type Job<T> = Box<dyn FnOnce(&mut T) + Send>;

/// Owns a value on a dedicated named thread, or on the caller's thread on
/// threadless WebAssembly. Calls finish before returning to the caller.
pub struct ThreadBound<T: 'static> {
    #[cfg(target_arch = "wasm32")]
    value: RefCell<T>,
    #[cfg(not(target_arch = "wasm32"))]
    jobs: Option<mpsc::Sender<Job<T>>>,
    #[cfg(not(target_arch = "wasm32"))]
    thread: Option<JoinHandle<()>>,
}

impl<T: 'static> ThreadBound<T> {
    /// Builds the value on its owning thread.
    #[cfg(not(target_arch = "wasm32"))]
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

    /// Builds the value on the current thread.
    #[cfg(target_arch = "wasm32")]
    pub fn spawn<E: Send + 'static>(
        _name: &str,
        init: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<Self, E> {
        Ok(Self {
            value: RefCell::new(init()?),
        })
    }

    /// Runs `f` on the owning thread and returns its result.
    ///
    /// # Panics
    /// Re-raises a panic from an earlier or the current call: the value is
    /// gone with its thread.
    #[cfg(not(target_arch = "wasm32"))]
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

    /// Runs `f` on the owning thread and returns its result.
    #[cfg(target_arch = "wasm32")]
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut T) -> R + Send + 'static) -> R {
        f(&mut self.value.borrow_mut())
    }
}

#[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[test]
    fn calls_share_the_owned_value() {
        let bound = ThreadBound::spawn("bound-state", || Ok::<_, ()>(Rc::new(1))).unwrap();
        assert_eq!(
            bound.call(|value| {
                *Rc::get_mut(value).unwrap() += 1;
                **value
            }),
            2
        );
        assert_eq!(bound.call(|value| **value), 2);
    }
}
