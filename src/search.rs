use crate::model::Snapshot;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const RESULT_LIMIT: usize = 500;
const DEBOUNCE: Duration = Duration::from_millis(150);

struct SearchReply {
    generation: u64,
    results: Vec<(usize, usize)>,
}

pub struct SearchState {
    pub query: String,
    pub error: Option<String>,
    pub results: Vec<(usize, usize)>,
    observed_query: String,
    revision: u64,
    generation: Arc<AtomicU64>,
    changed_at: Instant,
    submitted: bool,
    worker: Option<Receiver<SearchReply>>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            error: None,
            results: Vec::new(),
            observed_query: String::new(),
            revision: u64::MAX,
            generation: Arc::new(AtomicU64::new(0)),
            changed_at: Instant::now(),
            submitted: false,
            worker: None,
        }
    }
}

impl Drop for SearchState {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

impl SearchState {
    pub fn update(&mut self, nodes: Option<&Arc<Snapshot>>, revision: u64) -> bool {
        if self.query != self.observed_query || revision != self.revision {
            self.observed_query.clone_from(&self.query);
            self.revision = revision;
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.results.clear();
            self.error = None;
            self.changed_at = Instant::now();
            self.submitted = false;
        }
        if let Some(worker) = &self.worker {
            match worker.try_recv() {
                Ok(reply) => {
                    if reply.generation == self.generation.load(Ordering::Relaxed) {
                        self.results = reply.results;
                    }
                    self.worker = None;
                }
                Err(TryRecvError::Disconnected) => self.worker = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let query = self.query.trim();
        let Some(nodes) = nodes.filter(|_| !query.is_empty()) else {
            return self.worker.is_some();
        };
        if !self.submitted && self.worker.is_none() && self.changed_at.elapsed() >= DEBOUNCE {
            let nodes = Arc::clone(nodes);
            let query = query.to_lowercase();
            let generation = Arc::clone(&self.generation);
            let expected = generation.load(Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("windirstat-search".into())
                .spawn(move || {
                    let results = find_matches(&nodes, &query, &generation, expected);
                    let _ = tx.send(SearchReply {
                        generation: expected,
                        results,
                    });
                });
            match spawned {
                Ok(_) => self.worker = Some(rx),
                Err(error) => self.error = Some(error.to_string()),
            }
            self.submitted = true;
        }
        self.worker.is_some() || !self.submitted
    }
}

fn find_matches(
    nodes: &Snapshot,
    query: &str,
    generation: &AtomicU64,
    expected: u64,
) -> Vec<(usize, usize)> {
    let mut best = BinaryHeap::with_capacity(RESULT_LIMIT + 1);
    let mut visited = 0usize;
    nodes.visit_paths(|index, path| {
        if visited.is_multiple_of(256) && generation.load(Ordering::Relaxed) != expected {
            return std::ops::ControlFlow::Break(());
        }
        visited += 1;
        let path = path.to_string_lossy();
        let matches = if query.is_ascii() && path.is_ascii() {
            path.as_bytes()
                .windows(query.len())
                .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
        } else {
            path.to_lowercase().contains(query)
        };
        if matches {
            best.push(Reverse((nodes[index].size, index)));
            if best.len() > RESULT_LIMIT {
                best.pop();
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    if generation.load(Ordering::Relaxed) != expected {
        return Vec::new();
    }
    let mut best: Vec<_> = best.into_iter().map(|Reverse(value)| value).collect();
    best.sort_unstable_by(|a, b| b.cmp(a));
    best.into_iter().map(|(_, index)| (index, 0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;
    use std::path::PathBuf;

    fn fixture(count: usize) -> Arc<Snapshot> {
        let root = PathBuf::from("root");
        let mut builder = TreeBuilder::new(root.clone());
        for index in 0..count {
            builder.add(
                root.join(format!("file-{index}.dat")),
                false,
                index as u64 + 1,
                None,
                0,
            );
        }
        Arc::new(builder.finish())
    }

    #[test]
    fn search_keeps_largest_matches_with_a_bounded_result_set() {
        let nodes = fixture(600);
        let results = find_matches(&nodes, "file", &AtomicU64::new(0), 0);
        assert_eq!(results.len(), RESULT_LIMIT);
        assert_eq!(nodes[results[0].0].size, 600);
        assert_eq!(nodes[results[RESULT_LIMIT - 1].0].size, 101);
        assert!(find_matches(&nodes, "file", &AtomicU64::new(1), 0).is_empty());
        let mut builder = TreeBuilder::new("root".into());
        builder.add(PathBuf::from("root/K.txt"), false, 1, None, 0);
        builder.add(PathBuf::from("root/ПРИМЕР.txt"), false, 2, None, 0);
        let unicode = builder.finish();
        assert_eq!(find_matches(&unicode, "k", &AtomicU64::new(0), 0).len(), 1);
        assert_eq!(
            find_matches(&unicode, "пример", &AtomicU64::new(0), 0).len(),
            1
        );
    }

    #[test]
    fn changing_snapshot_rejects_old_worker_indices_even_when_query_is_identical() {
        let mut state = SearchState::default();
        state.query = "file".to_owned();
        state.observed_query = "file".to_owned();
        state.revision = 1;
        let (tx, rx) = mpsc::channel();
        state.worker = Some(rx);
        tx.send(SearchReply {
            generation: 0,
            results: vec![(999, 0)],
        })
        .unwrap();
        let smaller = fixture(2);
        state.update(Some(&smaller), 2);
        assert!(state.results.is_empty());
        assert!(state.worker.is_none());
    }
}
