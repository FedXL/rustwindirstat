use crate::model::{Node, TreeBuilder};
use crate::parallel::build_pool;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const WALK_BATCH_SIZE: usize = 8192;
const WALK_PARALLEL_THRESHOLD: u64 = 32_768;

struct ScanEntry {
    path: PathBuf,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    attributes: u32,
}

#[derive(Debug)]
pub enum ScanError {
    Cancelled,
    Failed(String),
}

impl From<String> for ScanError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Сканирование остановлено"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

#[cfg(windows)]
enum MftScanError {
    Cancelled,
    Failed {
        reason: String,
        elevation_required: bool,
    },
}

#[cfg(windows)]
impl MftScanError {
    fn from_ntfs(error: ntfs_reader::NtfsReaderError) -> Self {
        Self::Failed {
            elevation_required: matches!(error, ntfs_reader::NtfsReaderError::AccessDenied),
            reason: error.to_string(),
        }
    }
    fn other(reason: String) -> Self {
        Self::Failed {
            reason,
            elevation_required: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct ScanWarnings {
    pub skipped: u64,
    pub corrupt_records: u64,
    pub examples: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ScanTimings {
    preparation: Duration,
    enumeration: Duration,
    construction: Duration,
    finalization: Duration,
}

impl ScanTimings {
    pub fn describe(&self) -> String {
        format!(
            "Подготовка: {:.3} с\nЧтение и разбор: {:.3} с\nВставка записей: {:.3} с\nИтоги и сортировка: {:.3} с",
            self.preparation.as_secs_f64(),
            self.enumeration.as_secs_f64(),
            self.construction.as_secs_f64(),
            self.finalization.as_secs_f64()
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ScanPhase {
    Preparing,
    Walking,
    ReadingMft,
    Finalizing,
}

impl ScanPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => "Подготовка индекса MFT",
            Self::Walking => "Обход каталогов",
            Self::ReadingMft => "Чтение MFT",
            Self::Finalizing => "Итоги и сортировка дерева",
        }
    }
}

impl ScanWarnings {
    fn record(&mut self, error: impl std::fmt::Display) {
        self.skipped += 1;
        if self.examples.len() < 8 {
            self.examples.push(error.to_string());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    NtfsMft,
    DirectoryWalk,
}

impl ScanMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::NtfsMft => "Быстрое сканирование NTFS (MFT)",
            Self::DirectoryWalk => "Обычный обход папок",
        }
    }
}

#[derive(Debug)]
pub struct ScanResult {
    pub nodes: Arc<Vec<Node>>,
    pub mode: ScanMode,
    pub files: u64,
    pub elapsed: Duration,
    pub warnings: ScanWarnings,
    pub timings: ScanTimings,
}

#[derive(Debug)]
pub enum ScanEvent {
    Progress {
        files: u64,
        processed: u64,
        total: Option<u64>,
        bytes: u64,
        phase: ScanPhase,
    },
    MftFallback {
        reason: String,
        elevation_required: bool,
    },
    Finished(Result<ScanResult, ScanError>),
}

pub struct ScanTask {
    receiver: Receiver<ScanEvent>,
    cancel: Arc<AtomicBool>,
    _thread: std::thread::JoinHandle<()>,
}

impl ScanTask {
    pub fn try_recv(&self) -> Result<ScanEvent, TryRecvError> {
        self.receiver.try_recv()
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for ScanTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn scan_async(root: PathBuf) -> Result<ScanTask, ScanError> {
    let root = crate::paths::normalize(&root);
    let (tx, receiver) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let thread = std::thread::Builder::new()
        .name("windirstat-scan".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                scan(&root, &tx, &worker_cancel)
            }))
            .unwrap_or_else(|_| Err(ScanError::Failed("Scan worker failed".to_owned())));
            let _ = tx.send(ScanEvent::Finished(result));
        })
        .map_err(|error| ScanError::Failed(error.to_string()))?;
    Ok(ScanTask {
        receiver,
        cancel,
        _thread: thread,
    })
}

fn scan(root: &Path, tx: &Sender<ScanEvent>, cancel: &AtomicBool) -> Result<ScanResult, ScanError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    let metadata = std::fs::metadata(root)
        .map_err(|error| ScanError::Failed(format!("{}: {error}", root.display())))?;
    if !metadata.is_dir() {
        return Err(ScanError::Failed(format!(
            "Это не папка: {}",
            root.display()
        )));
    }

    #[cfg(windows)]
    if is_drive_root(root) {
        match scan_mft(root, tx, cancel) {
            Ok(result) => return Ok(result),
            Err(MftScanError::Cancelled) => return Err(ScanError::Cancelled),
            Err(MftScanError::Failed {
                reason,
                elevation_required,
            }) => {
                let _ = tx.send(ScanEvent::MftFallback {
                    reason,
                    elevation_required,
                });
            }
        }
    }

    scan_walk(root, tx, cancel)
}

struct ParallelBuilder {
    pool: &'static rayon::ThreadPool,
    trees: Vec<TreeBuilder>,
    staging: Vec<Vec<ScanEntry>>,
}

struct WalkBuilder {
    root: PathBuf,
    records: Vec<ScanEntry>,
    parallel: Option<ParallelBuilder>,
    construction: Duration,
}

impl WalkBuilder {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            records: Vec::with_capacity(WALK_BATCH_SIZE),
            parallel: None,
            construction: Duration::ZERO,
        }
    }

    fn push(&mut self, record: ScanEntry) -> Result<(), ScanError> {
        self.records.push(record);
        if self.parallel.is_none() && self.records.len() >= WALK_PARALLEL_THRESHOLD as usize {
            let pool = build_pool()?;
            self.parallel = Some(ParallelBuilder {
                pool,
                staging: (0..pool.current_num_threads())
                    .map(|_| Vec::new())
                    .collect(),
                trees: (0..pool.current_num_threads())
                    .map(|_| TreeBuilder::new(self.root.clone()))
                    .collect(),
            });
        }
        if let Some(parallel) = &mut self.parallel
            && self.records.len() >= WALK_BATCH_SIZE
        {
            let started = Instant::now();
            insert_records(
                &mut self.records,
                &self.root,
                parallel.pool,
                &mut parallel.trees,
                &mut parallel.staging,
            );
            self.construction += started.elapsed();
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<Node> {
        if let Some(mut parallel) = self.parallel {
            insert_records(
                &mut self.records,
                &self.root,
                parallel.pool,
                &mut parallel.trees,
                &mut parallel.staging,
            );
            let shards = parallel.pool.install(|| {
                parallel
                    .trees
                    .into_par_iter()
                    .map(TreeBuilder::finish)
                    .collect()
            });
            TreeBuilder::merge_finished_shards(shards)
        } else {
            let mut tree = TreeBuilder::new(self.root);
            for record in self.records {
                tree.add(
                    record.path,
                    record.is_dir,
                    record.size,
                    record.modified,
                    record.attributes,
                );
            }
            tree.finish()
        }
    }
}

fn scan_walk(
    root: &Path,
    tx: &Sender<ScanEvent>,
    cancel: &AtomicBool,
) -> Result<ScanResult, ScanError> {
    let started = Instant::now();
    let mut warnings = ScanWarnings::default();
    let mut builder = WalkBuilder::new(root.to_path_buf());
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut last_report = Instant::now();

    for entry in WalkDir::new(root).follow_links(false) {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.record(error);
                continue;
            }
        };
        if entry.path() == root {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.record(error);
                continue;
            }
        };
        let is_dir = metadata.is_dir();
        let size = if is_dir { 0 } else { metadata.len() };
        if !is_dir {
            files += 1;
            bytes = bytes.saturating_add(size);
        }
        let record = ScanEntry {
            path: entry.into_path(),
            is_dir,
            size,
            modified: metadata.modified().ok(),
            attributes: file_attributes(&metadata),
        };
        builder.push(record)?;
        if last_report.elapsed() >= Duration::from_millis(120) {
            let _ = tx.send(ScanEvent::Progress {
                files,
                processed: files,
                total: None,
                bytes,
                phase: ScanPhase::Walking,
            });
            last_report = Instant::now();
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    let mut timings = ScanTimings {
        construction: builder.construction,
        enumeration: started.elapsed().saturating_sub(builder.construction),
        ..ScanTimings::default()
    };
    let finishing = Instant::now();
    let nodes = builder.finish();
    timings.finalization = finishing.elapsed();
    if cancel.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    let _ = tx.send(ScanEvent::Progress {
        files,
        processed: files,
        total: Some(files),
        bytes,
        phase: ScanPhase::Finalizing,
    });
    Ok(ScanResult {
        nodes: Arc::new(nodes),
        mode: ScanMode::DirectoryWalk,
        files,
        elapsed: started.elapsed(),
        warnings,
        timings,
    })
}

fn insert_records(
    records: &mut Vec<ScanEntry>,
    root: &Path,
    pool: &rayon::ThreadPool,
    trees: &mut [TreeBuilder],
    shards: &mut [Vec<ScanEntry>],
) {
    for record in records.drain(..) {
        let shard = shard_for(&record.path, root, trees.len());
        shards[shard].push(record);
    }
    pool.install(|| {
        shards
            .par_iter_mut()
            .zip(trees.par_iter_mut())
            .for_each(|(records, tree)| {
                for record in records.drain(..) {
                    tree.add(
                        record.path,
                        record.is_dir,
                        record.size,
                        record.modified,
                        record.attributes,
                    );
                }
            });
    })
}

fn shard_for(path: &Path, root: &Path, shard_count: usize) -> usize {
    let root_depth = root.components().count();
    let first_component = path
        .components()
        .nth(root_depth)
        .map(|component| component.as_os_str().as_encoded_bytes())
        .unwrap_or_else(|| path.as_os_str().as_encoded_bytes());
    let hash = first_component
        .iter()
        .copied()
        .fold(2_166_136_261_u32, |hash, byte| {
            hash.wrapping_mul(16_777_619) ^ byte.to_ascii_lowercase() as u32
        });
    hash as usize % shard_count
}

#[cfg(windows)]
fn scan_mft(
    root: &Path,
    tx: &Sender<ScanEvent>,
    cancel: &AtomicBool,
) -> Result<ScanResult, MftScanError> {
    use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, Volume};

    let started = Instant::now();
    let drive = drive_letter(root).ok_or_else(|| {
        MftScanError::other(format!(
            "Не удалось определить букву диска в пути: {}",
            root.display()
        ))
    })?;
    let volume_path = format!(r"\\.\{}:", drive);
    let _ = tx.send(ScanEvent::Progress {
        files: 0,
        processed: 0,
        total: None,
        bytes: 0,
        phase: ScanPhase::Preparing,
    });
    let volume = Volume::new(volume_path.as_str()).map_err(MftScanError::from_ntfs)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(MftScanError::Cancelled);
    }
    let mut scan = MftScan::new(volume).map_err(MftScanError::from_ntfs)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(MftScanError::Cancelled);
    }
    let total = Some(scan.record_count());
    let mut warnings = ScanWarnings {
        corrupt_records: scan.corrupt_records(),
        ..ScanWarnings::default()
    };
    let mut timings = ScanTimings {
        preparation: started.elapsed(),
        ..ScanTimings::default()
    };
    let mut processed = 0;
    let mut cache = DefaultPathCache::new();
    let pool = build_pool().map_err(|error| MftScanError::other(error.to_string()))?;
    let worker_count = pool.current_num_threads();
    let mut trees: Vec<_> = (0..worker_count)
        .map(|_| TreeBuilder::new(root.to_path_buf()))
        .collect();
    let mut shards: Vec<Vec<ScanEntry>> = (0..worker_count).map(|_| Vec::new()).collect();
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut last_report = Instant::now();

    loop {
        let reading = Instant::now();
        let chunk = scan.next_chunk().map_err(MftScanError::from_ntfs)?;
        timings.enumeration += reading.elapsed();
        let Some(chunk) = chunk else {
            break;
        };
        let decoding = Instant::now();
        if cancel.load(Ordering::Relaxed) {
            return Err(MftScanError::Cancelled);
        }
        for (position, file) in chunk.files().enumerate() {
            if position.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
                return Err(MftScanError::Cancelled);
            }
            let info = FileInfo::with_cache(&file, &mut cache);
            if let Some(path) = info.path {
                let path = crate::paths::normalize_owned(path);
                if path.starts_with(root) {
                    let modified = info.modified.and_then(system_time_from_unix);
                    let shard = shard_for(&path, root, worker_count);
                    shards[shard].push(ScanEntry {
                        path,
                        is_dir: info.is_directory,
                        size: info.size,
                        modified,
                        attributes: info.file_attributes,
                    });
                    if !info.is_directory {
                        files += 1;
                        bytes = bytes.saturating_add(info.size);
                    }
                } else {
                    warnings.record(format!("Путь вне корня: {}", path.display()));
                }
            } else {
                warnings.record("Не удалось разрешить путь записи MFT");
            }
            if last_report.elapsed() >= Duration::from_millis(120) {
                let _ = tx.send(ScanEvent::Progress {
                    files,
                    processed,
                    total,
                    bytes,
                    phase: ScanPhase::ReadingMft,
                });
                last_report = Instant::now();
            }
        }
        processed = chunk.records().end;
        timings.enumeration += decoding.elapsed();
        let constructing = Instant::now();
        pool.install(|| {
            shards
                .par_iter_mut()
                .zip(trees.par_iter_mut())
                .for_each(|(records, tree)| {
                    for record in records.drain(..) {
                        tree.add(
                            record.path,
                            record.is_dir,
                            record.size,
                            record.modified,
                            record.attributes,
                        );
                    }
                });
        });
        timings.construction += constructing.elapsed();
    }

    let _ = tx.send(ScanEvent::Progress {
        files,
        processed,
        total,
        bytes,
        phase: ScanPhase::Finalizing,
    });
    let finishing = Instant::now();
    let shards = pool.install(|| trees.into_par_iter().map(TreeBuilder::finish).collect());
    let nodes = TreeBuilder::merge_finished_shards(shards);
    timings.finalization = finishing.elapsed();
    if cancel.load(Ordering::Relaxed) {
        return Err(MftScanError::Cancelled);
    }
    Ok(ScanResult {
        nodes: Arc::new(nodes),
        mode: ScanMode::NtfsMft,
        files,
        elapsed: started.elapsed(),
        warnings,
        timings,
    })
}

fn system_time_from_unix(timestamp: time::OffsetDateTime) -> Option<SystemTime> {
    let seconds = timestamp.unix_timestamp();
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
}

fn file_attributes(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        0
    }
}

#[cfg(windows)]
fn is_drive_root(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Prefix(prefix))
        if matches!(prefix.kind(), std::path::Prefix::Disk(_)))
        && matches!(components.next(), Some(std::path::Component::RootDir))
        && components.next().is_none()
}

#[cfg(windows)]
fn drive_letter(path: &Path) -> Option<char> {
    match path.components().next()? {
        std::path::Component::Prefix(prefix) => match prefix.kind() {
            std::path::Prefix::Disk(letter) => Some(char::from(letter)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ScanEntry, file_attributes, insert_records, scan_walk, shard_for};
    use crate::model::TreeBuilder;
    use crate::parallel::build_pool;
    use crate::scanner::ScanEvent;
    use rayon::prelude::*;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Instant;
    use std::time::{SystemTime, UNIX_EPOCH};
    use walkdir::WalkDir;

    #[test]
    fn cancelled_scan_stops_before_validation_or_mft_fallback() {
        let (tx, rx) = mpsc::channel();
        let result = super::scan(Path::new("not-a-real-root"), &tx, &AtomicBool::new(true));
        assert!(matches!(result, Err(super::ScanError::Cancelled)));
        assert!(rx.try_recv().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn drive_root_detection_uses_path_components() {
        assert!(super::is_drive_root(Path::new(r"C:\")));
        assert!(super::is_drive_root(Path::new("C:/")));
        assert!(!super::is_drive_root(Path::new(r"C:")));
        assert!(!super::is_drive_root(Path::new(r"C:\Windows")));
        assert!(!super::is_drive_root(Path::new(r"\\server\share\")));
        assert_eq!(super::drive_letter(Path::new(r"C:\")), Some('C'));
    }

    #[cfg(windows)]
    #[test]
    fn mft_elevation_flag_uses_the_ntfs_error_variant() {
        let denied = super::MftScanError::from_ntfs(ntfs_reader::NtfsReaderError::AccessDenied);
        assert!(matches!(
            denied,
            super::MftScanError::Failed {
                elevation_required: true,
                ..
            }
        ));

        let invalid =
            super::MftScanError::from_ntfs(ntfs_reader::NtfsReaderError::InvalidVolumePath);
        assert!(matches!(
            invalid,
            super::MftScanError::Failed {
                elevation_required: false,
                ..
            }
        ));
    }

    #[test]
    fn paths_under_the_same_top_level_directory_share_a_shard() {
        let root = Path::new(r"C:\");
        assert_eq!(
            shard_for(Path::new(r"C:\Windows\System32\a.dll"), root, 5),
            shard_for(Path::new(r"C:\Windows\Fonts\b.ttf"), root, 5)
        );
    }

    #[test]
    fn paths_under_different_roots_are_stably_distributed() {
        let root = Path::new("root");
        let alpha = shard_for(Path::new("root/Alpha/a.bin"), root, 5);
        assert_eq!(alpha, shard_for(Path::new("root/alpha/b.bin"), root, 5));
        assert!(alpha < 5);
        assert!(shard_for(Path::new("root/Beta/a.bin"), root, 5) < 5);
    }

    #[test]
    fn parallel_folder_scan_preserves_sizes_and_counts() {
        let root = std::env::temp_dir().join(format!(
            "windirstat-perf-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Alpha/Nested")).unwrap();
        std::fs::create_dir_all(root.join("Beta")).unwrap();
        std::fs::write(root.join("Alpha/one.bin"), vec![1u8; 100]).unwrap();
        std::fs::write(root.join("Alpha/Nested/two.bin"), vec![2u8; 250]).unwrap();
        std::fs::write(root.join("Beta/three.bin"), vec![3u8; 50]).unwrap();

        let (tx, _rx) = mpsc::channel::<ScanEvent>();
        let cancel = AtomicBool::new(false);
        let result = scan_walk(&root, &tx, &cancel).unwrap();
        let root_node = &result.nodes[0];
        assert_eq!(result.files, 3);
        assert_eq!(root_node.size, 400);
        assert_eq!(root_node.files, 3);
        assert_eq!(root_node.folders, 3);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "manual performance comparison; creates a temporary 10k-file fixture"]
    fn compare_small_folder_scan_with_sequential_baseline() {
        let root = std::env::temp_dir().join(format!(
            "windirstat-perf-bench-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for directory in 0..40 {
            let folder = root.join(format!("Folder-{directory:02}"));
            std::fs::create_dir_all(&folder).unwrap();
            for file in 0..256 {
                std::fs::write(folder.join(format!("file-{file:03}.dat")), b"windirstat").unwrap();
            }
        }

        let started = Instant::now();
        let mut serial_tree = TreeBuilder::new(root.clone());
        for entry in WalkDir::new(&root).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if entry.path() == root {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let is_dir = metadata.is_dir();
            serial_tree.add(
                entry.into_path(),
                is_dir,
                if is_dir { 0 } else { metadata.len() },
                metadata.modified().ok(),
                file_attributes(&metadata),
            );
        }
        let serial_nodes = serial_tree.finish();
        let serial_elapsed = started.elapsed();

        let (tx, _rx) = mpsc::channel::<ScanEvent>();
        let cancel = AtomicBool::new(false);
        let parallel = scan_walk(&root, &tx, &cancel).unwrap();
        eprintln!(
            "folder benchmark: sequential {:.3}s, optimized {:.3}s, {:.2}x; files={}",
            serial_elapsed.as_secs_f64(),
            parallel.elapsed.as_secs_f64(),
            serial_elapsed.as_secs_f64() / parallel.elapsed.as_secs_f64(),
            parallel.files
        );
        assert_eq!(parallel.files, 40 * 256);
        assert_eq!(parallel.nodes[0].size, serial_nodes[0].size);
        assert_eq!(parallel.nodes[0].files, serial_nodes[0].files);
        assert_eq!(parallel.nodes[0].folders, serial_nodes[0].folders);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "manual release benchmark for sharded tree construction"]
    fn compare_parallel_tree_builder_with_sequential_baseline() {
        let root = Path::new("root");
        let mut records: Vec<_> = (0..40)
            .flat_map(|folder| {
                (0..2500).map(move |file| ScanEntry {
                    path: PathBuf::from(format!("root/Folder-{folder:02}/file-{file:04}.bin")),
                    is_dir: false,
                    size: (file % 4096 + 1) as u64,
                    modified: None,
                    attributes: 0,
                })
            })
            .collect();

        let started = Instant::now();
        let mut sequential = TreeBuilder::new(root.to_path_buf());
        for record in &records {
            sequential.add(
                record.path.clone(),
                record.is_dir,
                record.size,
                record.modified,
                record.attributes,
            );
        }
        let sequential = sequential.finish();
        let sequential_elapsed = started.elapsed();

        let pool = build_pool().unwrap();
        let mut trees: Vec<_> = (0..pool.current_num_threads())
            .map(|_| TreeBuilder::new(root.to_path_buf()))
            .collect();
        let started = Instant::now();
        let mut staging: Vec<_> = (0..trees.len()).map(|_| Vec::new()).collect();
        insert_records(&mut records, root, pool, &mut trees, &mut staging);
        let shards = pool.install(|| trees.into_par_iter().map(TreeBuilder::finish).collect());
        let parallel = TreeBuilder::merge_finished_shards(shards);
        let parallel_elapsed = started.elapsed();

        eprintln!(
            "tree builder benchmark: sequential {:.3}s, sharded {:.3}s, {:.2}x; nodes={}",
            sequential_elapsed.as_secs_f64(),
            parallel_elapsed.as_secs_f64(),
            sequential_elapsed.as_secs_f64() / parallel_elapsed.as_secs_f64(),
            parallel.len()
        );
        assert_eq!(parallel.len(), sequential.len());
        assert_eq!(parallel[0].size, sequential[0].size);
        assert_eq!(parallel[0].files, sequential[0].files);
        assert_eq!(parallel[0].folders, sequential[0].folders);
    }
}
