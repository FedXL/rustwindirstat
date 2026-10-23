use rayon::{ThreadPool, ThreadPoolBuilder};

pub fn worker_count() -> usize {
    let logical = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    (logical.saturating_mul(7) / 10).max(1)
}

pub fn build_pool() -> Result<ThreadPool, rayon::ThreadPoolBuildError> {
    ThreadPoolBuilder::new()
        .num_threads(worker_count())
        .thread_name(|index| format!("windirstat-worker-{index}"))
        .build()
}
