use crate::model::Node;
use eframe::egui::{Color32, ColorImage};
use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

const MEMORY_LIMIT: usize = 192 * 1024 * 1024;
const MEMORY_RESERVE: usize = 8 * 1024 * 1024;
const CPU_WINDOW: Duration = Duration::from_millis(100);
const CPU_BUDGET: Duration = Duration::from_millis(30);
const CHECK_EVERY: usize = 256;
pub const PHASE_PREPARE: u8 = 1;
pub const PHASE_LAYOUT: u8 = 2;
pub const PHASE_DONE: u8 = 3;
pub const PHASE_LIMIT: u8 = 4;

#[derive(Clone, Copy, Default)]
pub struct PackedRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

pub struct RenderedMap {
    pub image: ColorImage,
    pub hits: Vec<u32>,
    pub rects: Vec<PackedRect>,
}

#[derive(Default)]
pub struct RenderProgress {
    phase: AtomicU8,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl RenderProgress {
    pub fn snapshot(&self) -> (u8, usize, usize) {
        (
            self.phase.load(Ordering::Relaxed),
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone, Copy, Default)]
struct R {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}
#[derive(Clone, Copy)]
struct Job {
    parent: u32,
    bounds: R,
}

struct CpuPacer {
    window: Instant,
    last: Instant,
    active: Duration,
    operations: usize,
}

impl CpuPacer {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            window: now,
            last: now,
            active: Duration::ZERO,
            operations: 0,
        }
    }
    fn checkpoint(&mut self, generation: &AtomicU64, expected: u64, force: bool) -> bool {
        if !force {
            self.operations += 1;
            if self.operations % CHECK_EVERY != 0 {
                return generation.load(Ordering::Relaxed) == expected;
            }
        }
        if generation.load(Ordering::Relaxed) != expected {
            return false;
        }
        let now = Instant::now();
        self.active += now.duration_since(self.last);
        self.last = now;
        if self.active >= CPU_BUDGET {
            while self.window.elapsed() < CPU_WINDOW {
                if generation.load(Ordering::Relaxed) != expected {
                    return false;
                }
                thread::sleep(Duration::from_millis(2).min(CPU_WINDOW - self.window.elapsed()));
            }
            self.window = Instant::now();
            self.last = self.window;
            self.active = Duration::ZERO;
        } else if self.window.elapsed() >= CPU_WINDOW {
            self.window = now;
            self.active = Duration::ZERO;
        }
        true
    }
}

pub fn render(
    nodes: Arc<Vec<Node>>,
    requested_width: usize,
    requested_height: usize,
    generation: Arc<AtomicU64>,
    expected: u64,
    progress: Arc<RenderProgress>,
) -> Option<RenderedMap> {
    if requested_width == 0 || requested_height == 0 {
        return None;
    }
    let node_count = nodes.len();
    let fixed_bytes = node_count.checked_mul(size_of::<u32>() + size_of::<PackedRect>())?;
    if fixed_bytes + MEMORY_RESERVE >= MEMORY_LIMIT {
        progress.phase.store(PHASE_LIMIT, Ordering::Relaxed);
        return None;
    }
    let desired_pixels = requested_width.checked_mul(requested_height)?;
    let max_pixels = (MEMORY_LIMIT - fixed_bytes - MEMORY_RESERVE) / 9;
    let pixels = desired_pixels.min(max_pixels).max(1);
    let scale = (pixels as f64 / desired_pixels as f64).sqrt().min(1.0);
    let width = ((requested_width as f64 * scale).round() as usize).max(1);
    let height = ((requested_height as f64 * scale).round() as usize).max(1);
    let pixel_count = width.checked_mul(height)?;
    let used = fixed_bytes + pixel_count * 9 + MEMORY_RESERVE;
    let job_capacity = MEMORY_LIMIT.saturating_sub(used) / size_of::<Job>();

    let mut pixels = vec![Color32::from_rgb(250, 250, 250); pixel_count];
    let mut hits = vec![u32::MAX; pixel_count];
    let mut hit_weights = vec![0u8; pixel_count];
    let mut representatives: Vec<u32> = (0..node_count)
        .map(|i| i.min(u32::MAX as usize) as u32)
        .collect();
    let mut rects = vec![PackedRect::default(); node_count];
    let mut colors: HashMap<u32, Color32> = HashMap::new();
    let mut pacer = CpuPacer::new();
    progress.phase.store(PHASE_PREPARE, Ordering::Relaxed);
    progress.total.store(node_count, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    for i in (0..node_count).rev() {
        let mut best = i;
        for &child in &nodes[i].children {
            if nodes[child].size > nodes[best].size {
                best = child;
            }
            if !pacer.checkpoint(&generation, expected, false) {
                return None;
            }
        }
        representatives[i] = best as u32;
        if i % CHECK_EVERY == 0 && !pacer.checkpoint(&generation, expected, false) {
            return None;
        }
        if i % 4096 == 0 || i == 0 {
            progress.done.store(node_count - i, Ordering::Relaxed);
        }
    }
    if !pacer.checkpoint(&generation, expected, true) {
        return None;
    }

    progress.phase.store(PHASE_LAYOUT, Ordering::Relaxed);
    progress
        .total
        .store(node_count.saturating_sub(1).max(1), Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let mut represented = 0usize;
    let mut jobs: Vec<Job> = Vec::with_capacity(job_capacity);
    let mut current = Some(Job {
        parent: 0,
        bounds: R {
            x: 0.0,
            y: 0.0,
            w: width as f64,
            h: height as f64,
        },
    });
    while current.is_some() || !jobs.is_empty() {
        let job = if let Some(job) = current.take() {
            job
        } else {
            jobs.pop().unwrap()
        };
        let parent = job.parent as usize;
        if job.bounds.w * job.bounds.h < 1.0 {
            let leaf = representatives[parent] as usize;
            rects[leaf] = pack_rect(job.bounds, width, height);
            let color = color_for(&nodes[leaf], &mut colors);
            raster_tile(
                &mut pixels,
                &mut hits,
                &mut hit_weights,
                width,
                height,
                leaf,
                job.bounds,
                color,
                &mut pacer,
                &generation,
                expected,
            )?;
            let count = (nodes[parent].files + nodes[parent].folders).saturating_sub(1) as usize;
            represented = represented.saturating_add(count);
            progress.done.store(represented, Ordering::Relaxed);
            continue;
        }
        let children = &nodes[parent].children;
        if children.is_empty() {
            continue;
        }
        let mut total = 0u128;
        for &i in children {
            total = total.saturating_add(nodes[i].size as u128);
            if !pacer.checkpoint(&generation, expected, false) {
                return None;
            }
        }
        if total == 0 {
            let count = (nodes[parent].files + nodes[parent].folders).saturating_sub(1) as usize;
            represented = represented.saturating_add(count);
            progress.done.store(represented, Ordering::Relaxed);
            continue;
        }
        let parent_bounds = job.bounds;
        let denominator = total as f64;
        let mut callback_error = false;
        squarify(&nodes, children, parent_bounds, denominator, |index, r| {
            if callback_error {
                return;
            }
            if !pacer.checkpoint(&generation, expected, false) {
                callback_error = true;
                return;
            }
            let node = &nodes[index];
            if node.size == 0 {
                represented = represented.saturating_add((node.files + node.folders) as usize);
            } else if node.is_dir && !node.children.is_empty() {
                rects[index] = pack_rect(r, width, height);
                represented = represented.saturating_add(1);
                if r.w * r.h < 1.0 || jobs.len() == job_capacity {
                    let leaf = representatives[index] as usize;
                    rects[leaf] = pack_rect(r, width, height);
                    let color = color_for(&nodes[leaf], &mut colors);
                    if raster_tile(
                        &mut pixels,
                        &mut hits,
                        &mut hit_weights,
                        width,
                        height,
                        leaf,
                        r,
                        color,
                        &mut pacer,
                        &generation,
                        expected,
                    )
                    .is_none()
                    {
                        callback_error = true;
                        return;
                    }
                    represented = represented
                        .saturating_add((node.files + node.folders).saturating_sub(1) as usize);
                } else {
                    jobs.push(Job {
                        parent: index as u32,
                        bounds: r,
                    });
                }
            } else {
                rects[index] = pack_rect(r, width, height);
                let color = color_for(node, &mut colors);
                if raster_tile(
                    &mut pixels,
                    &mut hits,
                    &mut hit_weights,
                    width,
                    height,
                    index,
                    r,
                    color,
                    &mut pacer,
                    &generation,
                    expected,
                )
                .is_none()
                {
                    callback_error = true;
                    return;
                }
                represented = represented.saturating_add(1);
            }
            if represented & 1023 == 0 {
                progress.done.store(represented, Ordering::Relaxed);
            }
        });
        if callback_error || generation.load(Ordering::Relaxed) != expected {
            return None;
        }
        progress.done.store(
            represented.min(progress.total.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        current = jobs.pop();
    }
    if generation.load(Ordering::Relaxed) != expected {
        return None;
    }
    progress.phase.store(PHASE_DONE, Ordering::Relaxed);
    progress
        .done
        .store(progress.total.load(Ordering::Relaxed), Ordering::Relaxed);
    Some(RenderedMap {
        image: ColorImage::new([width, height], pixels),
        hits,
        rects,
    })
}

fn pack_rect(r: R, width: usize, height: usize) -> PackedRect {
    let x = r.x.floor().max(0.0) as usize;
    let y = r.y.floor().max(0.0) as usize;
    let right = (r.x + r.w).ceil().min(width as f64) as usize;
    let bottom = (r.y + r.h).ceil().min(height as f64) as usize;
    PackedRect {
        x: x.min(u16::MAX as usize) as u16,
        y: y.min(u16::MAX as usize) as u16,
        w: right.saturating_sub(x).min(u16::MAX as usize) as u16,
        h: bottom.saturating_sub(y).min(u16::MAX as usize) as u16,
    }
}

fn squarify<F: FnMut(usize, R)>(
    nodes: &[Node],
    children: &[usize],
    bounds: R,
    mut remaining: f64,
    mut emit: F,
) {
    let mut area = bounds;
    let mut pos = 0usize;
    while pos < children.len() && area.w > 0.05 && area.h > 0.05 && remaining > 0.0 {
        while pos < children.len() && nodes[children[pos]].size == 0 {
            let i = children[pos];
            emit(i, R::default());
            pos += 1;
        }
        if pos >= children.len() {
            break;
        }
        let side = area.w.min(area.h);
        let area_total = area.w * area.h;
        let start = pos;
        let mut sum = 0.0f64;
        let mut min = f64::INFINITY;
        let mut max = 0.0f64;
        let mut best = f64::INFINITY;
        while pos < children.len() {
            if nodes[children[pos]].size == 0 {
                break;
            }
            let weight = nodes[children[pos]].size as f64;
            let ns = sum + weight;
            let nmin = min.min(weight);
            let nmax = max.max(weight);
            let score = worst(nmin, nmax, ns, side, area_total);
            if pos > start && score > best {
                break;
            }
            sum = ns;
            min = nmin;
            max = nmax;
            best = score;
            pos += 1;
        }
        if sum <= 0.0 {
            break;
        }
        let fraction = (sum / remaining).clamp(0.0, 1.0);
        if area.w >= area.h {
            let strip_h = (area.h * fraction).min(area.h);
            let mut x = area.x;
            for &i in &children[start..pos] {
                let w = area.w * (nodes[i].size as f64 / sum);
                emit(
                    i,
                    R {
                        x,
                        y: area.y,
                        w,
                        h: strip_h,
                    },
                );
                x += w;
            }
            area.y += strip_h;
            area.h -= strip_h;
        } else {
            let strip_w = (area.w * fraction).min(area.w);
            let mut y = area.y;
            for &i in &children[start..pos] {
                let h = area.h * (nodes[i].size as f64 / sum);
                emit(
                    i,
                    R {
                        x: area.x,
                        y,
                        w: strip_w,
                        h,
                    },
                );
                y += h;
            }
            area.x += strip_w;
            area.w -= strip_w;
        }
        remaining = (remaining - sum).max(0.0);
    }
    while pos < children.len() {
        let i = children[pos];
        emit(i, R::default());
        pos += 1;
    }
}
fn worst(min: f64, max: f64, sum: f64, side: f64, area: f64) -> f64 {
    if min <= 0.0 || sum <= 0.0 || area <= 0.0 {
        f64::INFINITY
    } else {
        (side * side * sum / (area * min)).max(area * max / (sum * side * side))
    }
}

fn raster_tile(
    pixels: &mut [Color32],
    hits: &mut [u32],
    weights: &mut [u8],
    width: usize,
    height: usize,
    index: usize,
    r: R,
    base: Color32,
    pacer: &mut CpuPacer,
    generation: &AtomicU64,
    expected: u64,
) -> Option<()> {
    if r.w <= 0.0 || r.h <= 0.0 {
        return Some(());
    }
    let x0 = r.x.floor().max(0.0) as usize;
    let y0 = r.y.floor().max(0.0) as usize;
    let x1 = (r.x + r.w).ceil().min(width as f64) as usize;
    let y1 = (r.y + r.h).ceil().min(height as f64) as usize;
    for y in y0..y1 {
        for x in x0..x1 {
            if !pacer.checkpoint(generation, expected, false) {
                return None;
            }
            let coverage = ((r.x + r.w).min((x + 1) as f64) - r.x.max(x as f64)).max(0.0)
                * ((r.y + r.h).min((y + 1) as f64) - r.y.max(y as f64)).max(0.0);
            if coverage <= 0.0 {
                continue;
            }
            let u = ((x as f64 + 0.5 - r.x) / r.w).clamp(0.0, 1.0);
            let v = ((y as f64 + 0.5 - r.y) / r.h).clamp(0.0, 1.0);
            let cushion = 0.68
                + 0.72
                    * (1.0 - ((u - 0.5).abs() * 2.0).powi(2))
                    * (1.0 - ((v - 0.5).abs() * 2.0).powi(2));
            let edge =
                if r.w > 2.0 && r.h > 2.0 && (u < 0.008 || v < 0.008 || u > 0.992 || v > 0.992) {
                    0.78
                } else {
                    1.0
                };
            let f = cushion * edge;
            let src = [
                (base.r() as f64 * f).min(255.0),
                (base.g() as f64 * f).min(255.0),
                (base.b() as f64 * f).min(255.0),
            ];
            let at = y * width + x;
            let old = pixels[at];
            let a = coverage.min(1.0);
            pixels[at] = Color32::from_rgb(
                (old.r() as f64 * (1.0 - a) + src[0] * a).round() as u8,
                (old.g() as f64 * (1.0 - a) + src[1] * a).round() as u8,
                (old.b() as f64 * (1.0 - a) + src[2] * a).round() as u8,
            );
            let weight = (coverage * 255.0).round().clamp(1.0, 255.0) as u8;
            if weight >= weights[at] {
                weights[at] = weight;
                hits[at] = index.min(u32::MAX as usize) as u32;
            }
        }
    }
    Some(())
}

fn color_for(node: &Node, cache: &mut HashMap<u32, Color32>) -> Color32 {
    let hash = node
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .fold(2166136261u32, |h, byte| {
            h.wrapping_mul(16777619) ^ byte as u32
        });
    if let Some(color) = cache.get(&hash) {
        return *color;
    }
    let color = extension_color(hash);
    if cache.len() < 2048 {
        cache.insert(hash, color);
    }
    color
}
fn extension_color(hash: u32) -> Color32 {
    let h = (hash % 360) as f32;
    let s = 0.68f32;
    let l = 0.53f32;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}
