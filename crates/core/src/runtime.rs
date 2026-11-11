//! A process-wide tokio runtime. GPUI has its own executor, so all network I/O
//! runs here and the UI simply awaits the returned future.

use std::future::Future;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("cirrus-io")
            .enable_all()
            .build()
            .expect("failed to start tokio runtime")
    })
}

/// Runs `fut` on the tokio runtime. The returned future can be awaited from any executor.
pub fn spawn<T, F>(fut: F) -> impl Future<Output = Result<T>> + Send + 'static
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let handle = runtime().spawn(fut);
    async move { handle.await.map_err(|e| anyhow!("background task failed: {e}"))? }
}
