use rayon::{ThreadPool, ThreadPoolBuilder};
use std::sync::OnceLock;

pub fn worker_count() -> usize {
    let logical = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    (logical.saturating_mul(7) / 10).max(1)
}

pub fn build_pool() -> Result<&'static ThreadPool, String> {
    static POOL: OnceLock<Result<ThreadPool, String>> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPoolBuilder::new()
            .num_threads(worker_count())
            .thread_name(|index| format!("windirstat-worker-{index}"))
            .build()
            .map_err(|error| error.to_string())
    })
    .as_ref()
    .map_err(Clone::clone)
}
