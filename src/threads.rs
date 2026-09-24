//! Starting the app's long-lived named worker threads.

use std::thread::JoinHandle;

/// Spawn `f` on a thread called `name`. For the core workers started once at
/// startup (output, UI): without one of them the app cannot work, so there is
/// no fallback to take and the spawn failure panics. The panic message is the
/// thread's name followed by the OS error, so the panic hook's report says
/// which worker could not start.
#[allow(
    clippy::expect_used,
    reason = "a thread that cannot be spawned at startup is unrecoverable; the panic message is the only diagnostic there is"
)]
pub fn spawn_named<F, T>(name: &str, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawned_thread_carries_the_given_name() {
        let handle = spawn_named("qd-test-named", || {
            std::thread::current().name().map(str::to_string)
        });
        assert_eq!(handle.thread().name(), Some("qd-test-named"));
        assert_eq!(handle.join().unwrap().as_deref(), Some("qd-test-named"));
    }
}
