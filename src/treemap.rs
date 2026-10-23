use crate::model::Node;
use crate::parallel::{build_pool, worker_count};
use eframe::egui::{Color32, ColorImage};
use rayon::prelude::*;
use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const MEMORY_RESERVE: usize = 16 * 1024 * 1024;
const CPU_WINDOW: Duration = Duration::from_millis(100);
const CPU_BUDGET: Duration = Duration::from_millis(70);
const CHECK_EVERY: usize = 256;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum RenderPhase {
    #[default]
    Preparing = 0,
    Layout = 1,
    Raster = 2,
    Done = 3,
    Limited = 4,
}

#[derive(Debug)]
pub enum RenderError {
    Cancelled,
    MemoryLimit,
    InvalidDimensions,
    WorkerPool(String),
    WorkerPanicked,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Rendering cancelled"),
            Self::MemoryLimit => f.write_str("Treemap memory budget exceeded"),
            Self::InvalidDimensions => f.write_str("Invalid treemap dimensions"),
            Self::WorkerPool(message) => write!(f, "Cannot start rendering workers: {message}"),
            Self::WorkerPanicked => f.write_str("Treemap worker failed"),
        }
    }
}

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
    pub elapsed: Duration,
}

#[derive(Default)]
pub struct RenderProgress {
    phase: AtomicU8,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl RenderProgress {
    pub fn snapshot(&self) -> (RenderPhase, usize, usize) {
        (
            match self.phase.load(Ordering::Relaxed) {
                1 => RenderPhase::Layout,
                2 => RenderPhase::Raster,
                3 => RenderPhase::Done,
                4 => RenderPhase::Limited,
                _ => RenderPhase::Preparing,
            },
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

#[derive(Clone, Copy)]
struct PaintTile {
    index: u32,
    bounds: R,
    color: Color32,
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
            if !self.operations.is_multiple_of(CHECK_EVERY) {
                return true;
            }
        }
        if generation.load(Ordering::Relaxed) != expected {
            return false;
        }
        let now = Instant::now();
        self.active += now.duration_since(self.last);
        self.last = now;
        if self.active >= CPU_BUDGET {
            loop {
                let remaining = CPU_WINDOW.saturating_sub(self.window.elapsed());
                if remaining.is_zero() {
                    break;
                }
                if generation.load(Ordering::Relaxed) != expected {
                    return false;
                }
                thread::sleep(Duration::from_millis(2).min(remaining));
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
) -> Result<RenderedMap, RenderError> {
    if requested_width == 0 || requested_height == 0 || nodes.is_empty() {
        return Err(RenderError::InvalidDimensions);
    }
    if generation.load(Ordering::Relaxed) != expected {
        return Err(RenderError::Cancelled);
    }
    let started = Instant::now();
    let node_count = nodes.len();
    let fixed_bytes = node_count
        .checked_mul(size_of::<PackedRect>() + size_of::<PaintTile>())
        .ok_or(RenderError::MemoryLimit)?;
    if fixed_bytes.saturating_add(MEMORY_RESERVE) >= MEMORY_LIMIT {
        progress
            .phase
            .store(RenderPhase::Limited as u8, Ordering::Relaxed);
        return Err(RenderError::MemoryLimit);
    }
    requested_width
        .checked_mul(requested_height)
        .ok_or(RenderError::InvalidDimensions)?;
    let requested_width = requested_width.min(u16::MAX as usize);
    let requested_height = requested_height.min(u16::MAX as usize);
    let desired_pixels = requested_width * requested_height;
    let max_pixels = (MEMORY_LIMIT - fixed_bytes - MEMORY_RESERVE) / 9;
    if max_pixels == 0 {
        return Err(RenderError::MemoryLimit);
    }
    let pixels = desired_pixels.min(max_pixels).max(1);
    let scale = (pixels as f64 / desired_pixels as f64).sqrt().min(1.0);
    let width = ((requested_width as f64 * scale).floor() as usize).clamp(1, max_pixels);
    let height = ((requested_height as f64 * scale).floor() as usize).clamp(1, max_pixels / width);
    let pixel_count = width
        .checked_mul(height)
        .ok_or(RenderError::InvalidDimensions)?;
    let used = fixed_bytes + pixel_count * 9 + MEMORY_RESERVE;
    let job_capacity = (MEMORY_LIMIT.saturating_sub(used) / size_of::<Job>()).min(65_536);

    let mut pixels = filled_buffer(pixel_count, Color32::from_rgb(250, 250, 250))?;
    let mut hits = filled_buffer(pixel_count, u32::MAX)?;
    let mut hit_weights = filled_buffer(pixel_count, 0u8)?;
    let mut rects = filled_buffer(node_count, PackedRect::default())?;
    let mut paint_tiles = Vec::new();
    paint_tiles
        .try_reserve_exact(node_count)
        .map_err(|_| RenderError::MemoryLimit)?;
    let mut colors: HashMap<u32, Color32> = HashMap::new();
    let mut pacer = CpuPacer::new();
    progress
        .phase
        .store(RenderPhase::Preparing as u8, Ordering::Relaxed);
    progress.total.store(node_count, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    progress
        .phase
        .store(RenderPhase::Layout as u8, Ordering::Relaxed);
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
            let leaf = parent;
            rects[leaf] = pack_rect(job.bounds, width, height);
            let color = color_for(&nodes[leaf], &mut colors);
            paint_tiles.push(PaintTile {
                index: leaf as u32,
                bounds: job.bounds,
                color,
            });
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
                return Err(RenderError::Cancelled);
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
                    let leaf = index;
                    rects[leaf] = pack_rect(r, width, height);
                    let color = color_for(&nodes[leaf], &mut colors);
                    paint_tiles.push(PaintTile {
                        index: leaf as u32,
                        bounds: r,
                        color,
                    });
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
                paint_tiles.push(PaintTile {
                    index: index as u32,
                    bounds: r,
                    color,
                });
                represented = represented.saturating_add(1);
            }
            if represented & 1023 == 0 {
                progress.done.store(represented, Ordering::Relaxed);
            }
        });
        if callback_error || generation.load(Ordering::Relaxed) != expected {
            return Err(RenderError::Cancelled);
        }
        progress.done.store(
            represented.min(progress.total.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        current = jobs.pop();
    }
    if generation.load(Ordering::Relaxed) != expected {
        return Err(RenderError::Cancelled);
    }

    progress
        .phase
        .store(RenderPhase::Raster as u8, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    progress.total.store(height, Ordering::Relaxed);
    let mut row_counts = vec![0usize; height];
    let mut row_references = 0usize;
    for (tile_index, tile) in paint_tiles.iter().enumerate() {
        if tile_index % CHECK_EVERY == 0 && generation.load(Ordering::Relaxed) != expected {
            return Err(RenderError::Cancelled);
        }
        for y in tile_rows(tile.bounds, height) {
            row_counts[y] = row_counts[y].saturating_add(1);
            row_references = row_references.saturating_add(1);
        }
    }
    let row_index_bytes = row_references
        .saturating_mul(size_of::<usize>())
        .saturating_add(height.saturating_mul(size_of::<Vec<usize>>()));
    let jobs_bytes = job_capacity.saturating_mul(size_of::<Job>());
    let has_row_budget = used
        .saturating_add(jobs_bytes)
        .saturating_add(row_index_bytes)
        <= MEMORY_LIMIT;
    if has_row_budget && worker_count() > 1 {
        let mut rows: Vec<Vec<usize>> = row_counts.into_iter().map(Vec::with_capacity).collect();
        for (tile_index, tile) in paint_tiles.iter().enumerate() {
            if tile_index % CHECK_EVERY == 0 && generation.load(Ordering::Relaxed) != expected {
                return Err(RenderError::Cancelled);
            }
            for y in tile_rows(tile.bounds, height) {
                rows[y].push(tile_index);
            }
        }
        let pool = build_pool().map_err(RenderError::WorkerPool)?;
        if !raster_rows_parallel(
            RasterTarget {
                pixels: &mut pixels,
                hits: &mut hits,
                weights: &mut hit_weights,
                width,
                height,
            },
            &rows,
            &paint_tiles,
            RasterControl {
                generation: &generation,
                expected,
            },
            &progress,
            pool,
        ) {
            return Err(RenderError::Cancelled);
        }
    } else {
        progress.total.store(paint_tiles.len(), Ordering::Relaxed);
        for (done, tile) in paint_tiles.iter().enumerate() {
            raster_tile(
                &mut RasterTarget {
                    pixels: &mut pixels,
                    hits: &mut hits,
                    weights: &mut hit_weights,
                    width,
                    height,
                },
                *tile,
                &mut pacer,
                RasterControl {
                    generation: &generation,
                    expected,
                },
            )
            .ok_or(RenderError::Cancelled)?;
            progress.done.store(done + 1, Ordering::Relaxed);
        }
    }
    if generation.load(Ordering::Relaxed) != expected {
        return Err(RenderError::Cancelled);
    }
    progress
        .phase
        .store(RenderPhase::Done as u8, Ordering::Relaxed);
    progress
        .done
        .store(progress.total.load(Ordering::Relaxed), Ordering::Relaxed);
    Ok(RenderedMap {
        image: ColorImage::new([width, height], pixels),
        hits,
        rects,
        elapsed: started.elapsed(),
    })
}

fn filled_buffer<T: Clone>(length: usize, value: T) -> Result<Vec<T>, RenderError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length)
        .map_err(|_| RenderError::MemoryLimit)?;
    buffer.resize(length, value);
    Ok(buffer)
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

fn tile_rows(bounds: R, height: usize) -> Range<usize> {
    let y0 = bounds.y.floor().max(0.0) as usize;
    let y1 = (bounds.y + bounds.h).ceil().min(height as f64) as usize;
    y0.min(height)..y1.min(height)
}

struct RasterTarget<'a> {
    pixels: &'a mut [Color32],
    hits: &'a mut [u32],
    weights: &'a mut [u8],
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
struct RasterControl<'a> {
    generation: &'a AtomicU64,
    expected: u64,
}

fn raster_rows_parallel(
    target: RasterTarget<'_>,
    rows: &[Vec<usize>],
    tiles: &[PaintTile],
    control: RasterControl<'_>,
    progress: &RenderProgress,
    pool: &rayon::ThreadPool,
) -> bool {
    pool.install(|| {
        target
            .pixels
            .par_chunks_mut(target.width)
            .zip(target.hits.par_chunks_mut(target.width))
            .zip(target.weights.par_chunks_mut(target.width))
            .enumerate()
            .try_for_each(|(y, ((pixel_row, hit_row), weight_row))| {
                for &tile_index in &rows[y] {
                    if !raster_tile_row(
                        pixel_row,
                        hit_row,
                        weight_row,
                        y,
                        tiles[tile_index],
                        control,
                    ) {
                        return Err(());
                    }
                }
                progress.done.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
    })
    .is_ok()
}

fn raster_tile_row(
    pixels: &mut [Color32],
    hits: &mut [u32],
    weights: &mut [u8],
    y: usize,
    tile: PaintTile,
    control: RasterControl<'_>,
) -> bool {
    let r = tile.bounds;
    let width = pixels.len();
    let x0 = r.x.floor().max(0.0) as usize;
    let x1 = (r.x + r.w).ceil().min(width as f64) as usize;
    let y_f = y as f64;
    let vertical_coverage = ((r.y + r.h).min(y_f + 1.0) - r.y.max(y_f)).max(0.0);
    if vertical_coverage <= 0.0 || r.w <= 0.0 || r.h <= 0.0 {
        return true;
    }
    for (operations, x) in (x0..x1).enumerate() {
        if operations.is_multiple_of(CHECK_EVERY)
            && control.generation.load(Ordering::Relaxed) != control.expected
        {
            return false;
        }
        let horizontal_coverage = ((r.x + r.w).min((x + 1) as f64) - r.x.max(x as f64)).max(0.0);
        let coverage = horizontal_coverage * vertical_coverage;
        if coverage <= 0.0 {
            continue;
        }
        let u = ((x as f64 + 0.5 - r.x) / r.w).clamp(0.0, 1.0);
        let v = ((y_f + 0.5 - r.y) / r.h).clamp(0.0, 1.0);
        let cushion = 0.68
            + 0.72
                * (1.0 - ((u - 0.5).abs() * 2.0).powi(2))
                * (1.0 - ((v - 0.5).abs() * 2.0).powi(2));
        let edge = if r.w > 2.0 && r.h > 2.0 && (u < 0.008 || v < 0.008 || u > 0.992 || v > 0.992) {
            0.78
        } else {
            1.0
        };
        let factor = cushion * edge;
        let src = [
            (tile.color.r() as f64 * factor).min(255.0),
            (tile.color.g() as f64 * factor).min(255.0),
            (tile.color.b() as f64 * factor).min(255.0),
        ];
        let old = pixels[x];
        let alpha = coverage.min(1.0);
        pixels[x] = Color32::from_rgb(
            (old.r() as f64 * (1.0 - alpha) + src[0] * alpha).round() as u8,
            (old.g() as f64 * (1.0 - alpha) + src[1] * alpha).round() as u8,
            (old.b() as f64 * (1.0 - alpha) + src[2] * alpha).round() as u8,
        );
        let weight = (coverage * 255.0).round().clamp(1.0, 255.0) as u8;
        if weight >= weights[x] {
            weights[x] = weight;
            hits[x] = tile.index;
        }
    }
    true
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
            let score = worst(nmin, nmax, ns, side, area_total / remaining);
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
        } else {
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
        }
        remaining = (remaining - sum).max(0.0);
    }
    while pos < children.len() {
        let i = children[pos];
        emit(i, R::default());
        pos += 1;
    }
}
fn worst(min: f64, max: f64, sum: f64, side: f64, scale: f64) -> f64 {
    if min <= 0.0 || sum <= 0.0 || side <= 0.0 || scale <= 0.0 {
        return f64::INFINITY;
    }
    let sum_area = sum * scale;
    let sum_squared = sum_area * sum_area;
    (side * side * max * scale / sum_squared).max(sum_squared / (side * side * min * scale))
}

fn raster_tile(
    target: &mut RasterTarget<'_>,
    tile: PaintTile,
    pacer: &mut CpuPacer,
    control: RasterControl<'_>,
) -> Option<()> {
    for y in tile_rows(tile.bounds, target.height) {
        if !pacer.checkpoint(control.generation, control.expected, false) {
            return None;
        }
        let start = y * target.width;
        let end = start + target.width;
        if !raster_tile_row(
            &mut target.pixels[start..end],
            &mut target.hits[start..end],
            &mut target.weights[start..end],
            y,
            tile,
            control,
        ) {
            return None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;
    use std::time::Instant;

    fn fixture(sizes: &[u64]) -> Vec<Node> {
        let root = std::path::PathBuf::from("root");
        let mut builder = crate::model::TreeBuilder::new(root.clone());
        for (index, &size) in sizes.iter().enumerate() {
            builder.add(root.join(format!("{index}.bin")), false, size, None, 0);
        }
        builder.finish()
    }

    #[test]
    fn equal_files_produce_square_tiles() {
        let nodes = fixture(&[1, 1, 1, 1]);
        let mut tiles = Vec::new();
        squarify(
            &nodes,
            &nodes[0].children,
            R {
                w: 100.0,
                h: 100.0,
                ..R::default()
            },
            4.0,
            |_, rect| tiles.push(rect),
        );
        assert_eq!(tiles.len(), 4);
        assert!(
            tiles
                .iter()
                .all(|rect| (rect.w - 50.0).abs() < 1e-8 && (rect.h - 50.0).abs() < 1e-8)
        );
    }

    #[test]
    fn layout_preserves_area_and_does_not_overlap_on_skewed_and_tall_inputs() {
        for sizes in [vec![90, 5, 3, 2, 0], vec![1; 100], vec![0, 0]] {
            let nodes = fixture(&sizes);
            let total: u64 = sizes.iter().sum();
            for (w, h) in [(100.0, 100.0), (9.0, 1000.0), (1000.0, 9.0)] {
                let bounds = R {
                    x: 10.0,
                    y: 20.0,
                    w,
                    h,
                };
                let mut tiles = Vec::new();
                squarify(
                    &nodes,
                    &nodes[0].children,
                    bounds,
                    total as f64,
                    |index, rect| {
                        if rect.w * rect.h > 0.0 {
                            assert!(rect.x >= bounds.x - 1e-8 && rect.y >= bounds.y - 1e-8);
                            assert!(rect.x + rect.w <= bounds.x + w + 1e-8);
                            assert!(rect.y + rect.h <= bounds.y + h + 1e-8);
                            let expected = w * h * nodes[index].size as f64 / total as f64;
                            assert!((rect.w * rect.h - expected).abs() < 1e-6);
                            tiles.push(rect);
                        }
                    },
                );
                for (index, a) in tiles.iter().enumerate() {
                    for b in &tiles[index + 1..] {
                        let overlap_w = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0.0);
                        let overlap_h = ((a.y + a.h).min(b.y + b.h) - a.y.max(b.y)).max(0.0);
                        assert!(overlap_w * overlap_h < 1e-6);
                    }
                }
            }
        }
    }

    #[test]
    fn render_reports_cancellation_and_invalid_dimensions() {
        let nodes = Arc::new(fixture(&[1]));
        assert!(matches!(
            render(
                Arc::clone(&nodes),
                10,
                10,
                Arc::new(AtomicU64::new(1)),
                0,
                Arc::new(RenderProgress::default())
            ),
            Err(RenderError::Cancelled)
        ));
        assert!(matches!(
            render(
                nodes,
                usize::MAX,
                2,
                Arc::new(AtomicU64::new(0)),
                0,
                Arc::new(RenderProgress::default())
            ),
            Err(RenderError::InvalidDimensions)
        ));
        let mut pacer = CpuPacer::new();
        pacer.window = Instant::now() - Duration::from_secs(1);
        pacer.active = CPU_BUDGET;
        assert!(pacer.checkpoint(&AtomicU64::new(0), 0, true));
    }

    #[test]
    fn parallel_rows_match_serial_pixels_and_hit_testing() {
        let width = 32;
        let height = 24;
        let tiles = [
            PaintTile {
                index: 1,
                bounds: R {
                    x: 0.25,
                    y: 0.5,
                    w: 15.5,
                    h: 23.0,
                },
                color: Color32::from_rgb(210, 40, 60),
            },
            PaintTile {
                index: 2,
                bounds: R {
                    x: 15.75,
                    y: 0.5,
                    w: 16.0,
                    h: 10.5,
                },
                color: Color32::from_rgb(40, 180, 90),
            },
            PaintTile {
                index: 3,
                bounds: R {
                    x: 15.75,
                    y: 10.75,
                    w: 16.0,
                    h: 12.75,
                },
                color: Color32::from_rgb(40, 90, 210),
            },
        ];
        let white = Color32::from_rgb(250, 250, 250);
        let mut serial_pixels = vec![white; width * height];
        let mut serial_hits = vec![u32::MAX; width * height];
        let mut serial_weights = vec![0u8; width * height];
        let generation = AtomicU64::new(7);
        let mut pacer = CpuPacer::new();
        for tile in &tiles {
            raster_tile(
                &mut RasterTarget {
                    pixels: &mut serial_pixels,
                    hits: &mut serial_hits,
                    weights: &mut serial_weights,
                    width,
                    height,
                },
                *tile,
                &mut pacer,
                RasterControl {
                    generation: &generation,
                    expected: 7,
                },
            )
            .unwrap();
        }

        let mut row_tiles = vec![Vec::new(); height];
        for (index, tile) in tiles.iter().enumerate() {
            for y in tile_rows(tile.bounds, height) {
                row_tiles[y].push(index);
            }
        }
        let mut parallel_pixels = vec![white; width * height];
        let mut parallel_hits = vec![u32::MAX; width * height];
        let mut parallel_weights = vec![0u8; width * height];
        let progress = RenderProgress::default();
        progress
            .phase
            .store(RenderPhase::Raster as u8, Ordering::Relaxed);
        progress.total.store(height, Ordering::Relaxed);
        let pool = build_pool().unwrap();
        assert!(raster_rows_parallel(
            RasterTarget {
                pixels: &mut parallel_pixels,
                hits: &mut parallel_hits,
                weights: &mut parallel_weights,
                width,
                height
            },
            &row_tiles,
            &tiles,
            RasterControl {
                generation: &generation,
                expected: 7
            },
            &progress,
            pool,
        ));

        assert_eq!(parallel_pixels, serial_pixels);
        assert_eq!(parallel_hits, serial_hits);
        assert_eq!(parallel_weights, serial_weights);
        assert_eq!(progress.snapshot(), (RenderPhase::Raster, height, height));
    }

    #[test]
    fn render_pipeline_returns_clickable_tiles() {
        let root = std::path::PathBuf::from("root");
        let nodes = Arc::new(vec![
            Node {
                name: "root".to_owned(),
                path: root.clone(),
                size: 300,
                is_dir: true,
                parent: None,
                children: vec![1, 2],
                files: 2,
                folders: 0,
                modified: None,
                attributes: 0,
            },
            Node {
                name: "large.bin".to_owned(),
                path: root.join("large.bin"),
                size: 200,
                is_dir: false,
                parent: Some(0),
                children: Vec::new(),
                files: 1,
                folders: 0,
                modified: None,
                attributes: 0,
            },
            Node {
                name: "small.txt".to_owned(),
                path: root.join("small.txt"),
                size: 100,
                is_dir: false,
                parent: Some(0),
                children: Vec::new(),
                files: 1,
                folders: 0,
                modified: None,
                attributes: 0,
            },
        ]);
        let progress = Arc::new(RenderProgress::default());
        let rendered = render(
            nodes,
            64,
            48,
            Arc::new(AtomicU64::new(0)),
            0,
            progress.clone(),
        )
        .unwrap();

        assert_eq!(rendered.image.size, [64, 48]);
        assert!(rendered.hits.iter().all(|&hit| hit == 1 || hit == 2));
        assert_eq!(progress.snapshot().0, RenderPhase::Done);
        assert!(rendered.elapsed > Duration::ZERO);
    }

    #[test]
    #[ignore = "manual release benchmark for parallel pixel rasterization"]
    fn compare_parallel_raster_with_sequential_baseline() {
        let width = 1600;
        let height = 900;
        let tiles: Vec<_> = (0..32)
            .map(|index| PaintTile {
                index,
                bounds: R {
                    x: 0.0,
                    y: index as f64 * height as f64 / 32.0,
                    w: width as f64,
                    h: height as f64 / 32.0,
                },
                color: extension_color(index.wrapping_mul(7919)),
            })
            .collect();
        let white = Color32::from_rgb(250, 250, 250);
        let generation = AtomicU64::new(1);

        let started = Instant::now();
        let mut serial_pixels = vec![white; width * height];
        let mut serial_hits = vec![u32::MAX; width * height];
        let mut serial_weights = vec![0u8; width * height];
        let mut pacer = CpuPacer::new();
        for tile in &tiles {
            raster_tile(
                &mut RasterTarget {
                    pixels: &mut serial_pixels,
                    hits: &mut serial_hits,
                    weights: &mut serial_weights,
                    width,
                    height,
                },
                *tile,
                &mut pacer,
                RasterControl {
                    generation: &generation,
                    expected: 1,
                },
            )
            .unwrap();
        }
        let serial_elapsed = started.elapsed();

        let mut rows = vec![Vec::new(); height];
        for (tile_index, tile) in tiles.iter().enumerate() {
            for y in tile_rows(tile.bounds, height) {
                rows[y].push(tile_index);
            }
        }
        let mut parallel_pixels = vec![white; width * height];
        let mut parallel_hits = vec![u32::MAX; width * height];
        let mut parallel_weights = vec![0u8; width * height];
        let progress = RenderProgress::default();
        progress.total.store(height, Ordering::Relaxed);
        let pool = build_pool().unwrap();
        let started = Instant::now();
        assert!(raster_rows_parallel(
            RasterTarget {
                pixels: &mut parallel_pixels,
                hits: &mut parallel_hits,
                weights: &mut parallel_weights,
                width,
                height
            },
            &rows,
            &tiles,
            RasterControl {
                generation: &generation,
                expected: 1
            },
            &progress,
            pool,
        ));
        let parallel_elapsed = started.elapsed();

        assert_eq!(parallel_pixels, serial_pixels);
        assert_eq!(parallel_hits, serial_hits);
        assert_eq!(parallel_weights, serial_weights);
        eprintln!(
            "treemap raster benchmark: sequential {:.3}s, parallel {:.3}s, {:.2}x",
            serial_elapsed.as_secs_f64(),
            parallel_elapsed.as_secs_f64(),
            serial_elapsed.as_secs_f64() / parallel_elapsed.as_secs_f64()
        );
    }
}
