//! Signal's library cannot be moved between threads: the protocol stores it takes are written for a single thread
//! (`#[async_trait(?Send)]` in libsignal), so everything that runs a presage `Manager` has to stay on the thread it
//! started on. Every such piece of work gets a thread of its own with a single-threaded runtime and a `LocalSet`, and
//! talks to the rest of the daemon through channels and the shared state it is handed.

use std::future::Future;

/// Runs the future `make` builds on a new thread, and answers with what it returns. The future itself need not be
/// `Send`, which is the point: only `make` and the result cross threads.
pub fn run<T, F, Fut>(name: &str, make: F) -> tokio::sync::oneshot::Receiver<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::error!("signal: could not start a runtime: {e}");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        let out = runtime.block_on(local.run_until(make()));
        let _ = tx.send(out);
    });
    if let Err(e) = spawned {
        tracing::error!("signal: could not start a thread: {e}");
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[tokio::test]
    async fn work_that_is_not_send_runs_and_answers() {
        let answer = run("test", || async {
            // An `Rc` held across an await cannot be `Send`: this is what the library's futures are like.
            let shared = Rc::new(41);
            tokio::task::yield_now().await;
            *shared + 1
        })
        .await
        .unwrap();
        assert_eq!(answer, 42);
    }
}
