/// Coalesce bulk history/runtime releases without scanning glibc arenas on
/// Tokio workers or on the live-event path. The delay also lets callers drop
/// their last session/cell references after teardown returns.
pub(crate) fn release_unused() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        static PENDING: AtomicBool = AtomicBool::new(false);
        if PENDING.swap(true, Ordering::AcqRel) {
            return;
        }
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = tokio::task::spawn_blocking(|| {
                // SAFETY: glibc synchronizes its arenas internally. This only
                // returns unused pages; live allocations remain untouched.
                unsafe { libc::malloc_trim(0) };
                PENDING.store(false, Ordering::Release);
            })
            .await;
        });
    }
}
