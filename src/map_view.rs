use crate::model::Snapshot;
use crate::treemap::{self, PackedRect, RenderError, RenderProgress, RenderedMap};
use eframe::egui;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

struct RenderReply {
    generation: u64,
    requested_size: [usize; 2],
    result: Result<RenderedMap, RenderError>,
}

pub struct MapView {
    generation: Arc<AtomicU64>,
    receiver: Receiver<RenderReply>,
    sender: Sender<RenderReply>,
    running: Arc<AtomicBool>,
    requested_size: [usize; 2],
    finished_size: [usize; 2],
    requested_at: Instant,
    pub progress: Option<Arc<RenderProgress>>,
    pub size: [usize; 2],
    pub elapsed: Option<Duration>,
    pub texture: Option<egui::TextureHandle>,
    pub hits: Vec<u32>,
    pub rects: Vec<PackedRect>,
    pub error: Option<RenderError>,
}

impl Default for MapView {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            receiver,
            sender,
            running: Arc::new(AtomicBool::new(false)),
            requested_size: [0; 2],
            finished_size: [0; 2],
            requested_at: Instant::now(),
            progress: None,
            size: [0; 2],
            elapsed: None,
            texture: None,
            hits: Vec::new(),
            rects: Vec::new(),
            error: None,
        }
    }
}

impl Drop for MapView {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

impl MapView {
    pub fn invalidate(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.size = [0; 2];
        self.requested_size = [0; 2];
        self.finished_size = [0; 2];
        self.elapsed = None;
        self.texture = None;
        self.hits.clear();
        self.rects.clear();
        self.progress = None;
        self.error = None;
    }

    pub fn poll(&mut self, ctx: &egui::Context, nodes: &Arc<Snapshot>, size: [usize; 2]) {
        while let Ok(reply) = self.receiver.try_recv() {
            if reply.generation != self.generation.load(Ordering::Relaxed) {
                continue;
            }
            match reply.result {
                Ok(map) => {
                    self.elapsed = Some(map.elapsed);
                    self.size = map.image.size;
                    if let Some(texture) = &mut self.texture {
                        texture.set(map.image, egui::TextureOptions::LINEAR);
                    } else {
                        self.texture = Some(ctx.load_texture(
                            "treemap",
                            map.image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                    self.finished_size = reply.requested_size;
                    self.hits = map.hits;
                    self.rects = map.rects;
                }
                Err(RenderError::Cancelled) => {}
                Err(error) => self.error = Some(error),
            }
        }
        if size.contains(&0) {
            return;
        }
        if size != self.requested_size {
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.progress = None;
            self.error = None;
            self.requested_size = size;
            self.elapsed = None;
            self.requested_at = Instant::now();
        }
        if self.finished_size != size
            && self.error.is_none()
            && self.requested_at.elapsed() >= Duration::from_millis(140)
            && self
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            let expected = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
            let nodes = Arc::clone(nodes);
            let sender = self.sender.clone();
            let generation = Arc::clone(&self.generation);
            let running = Arc::clone(&self.running);
            let progress = Arc::new(RenderProgress::default());
            self.progress = Some(Arc::clone(&progress));
            let spawned = std::thread::Builder::new()
                .name("windirstat-render".into())
                .spawn(move || {
                    struct RunningGuard(Arc<AtomicBool>);
                    impl Drop for RunningGuard {
                        fn drop(&mut self) {
                            self.0.store(false, Ordering::Release);
                        }
                    }
                    let _guard = RunningGuard(running);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        treemap::render(nodes, size[0], size[1], generation, expected, progress)
                    }))
                    .unwrap_or(Err(RenderError::WorkerPanicked));
                    let _ = sender.send(RenderReply {
                        generation: expected,
                        requested_size: size,
                        result,
                    });
                });
            if let Err(error) = spawned {
                self.running.store(false, Ordering::Release);
                self.error = Some(RenderError::WorkerPool(error.to_string()));
            }
        }
        if self.running.load(Ordering::Relaxed)
            || (self.finished_size != size && self.error.is_none())
        {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_failure_does_not_automatically_restart_the_same_request() {
        let mut view = MapView::default();
        view.requested_size = [20, 20];
        view.requested_at = Instant::now() - Duration::from_secs(1);
        view.sender
            .send(RenderReply {
                generation: 0,
                requested_size: [20, 20],
                result: Err(RenderError::MemoryLimit),
            })
            .unwrap();
        let nodes = Arc::new(crate::model::TreeBuilder::new("root".into()).finish());
        for _ in 0..3 {
            view.poll(&egui::Context::default(), &nodes, [20, 20]);
        }
        assert!(matches!(view.error, Some(RenderError::MemoryLimit)));
        assert!(!view.running.load(Ordering::Relaxed));
    }
}
