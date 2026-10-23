use crate::model::{Node, TreeBuilder};
use crate::parallel::build_pool;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
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
}

#[derive(Debug)]
pub enum ScanEvent {
    Progress {
        files: u64,
        total: Option<u64>,
        bytes: u64,
        phase: String,
    },
    MftFallback {
        reason: String,
        elevation_required: bool,
    },
    Finished(Result<ScanResult, String>),
}

pub fn scan_async(root: PathBuf, tx: Sender<ScanEvent>, cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let result = scan(&root, &tx, &cancel);
        let _ = tx.send(ScanEvent::Finished(result));
    });
}

fn scan(root: &Path, tx: &Sender<ScanEvent>, cancel: &AtomicBool) -> Result<ScanResult, String> {
    if !root.exists() {
        return Err(format!("Путь не найден: {}", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("Это не папка: {}", root.display()));
    }

    #[cfg(windows)]
    if is_drive_root(root) {
        match scan_mft(root, tx, cancel) {
            Ok(result) => return Ok(result),
            Err(error) => {
                let _ = tx.send(ScanEvent::MftFallback {
                    elevation_required: is_access_denied(&error),
                    reason: error,
                });
            }
        }
    }

    scan_walk(root, tx, cancel)
}

fn scan_walk(
    root: &Path,
    tx: &Sender<ScanEvent>,
    cancel: &AtomicBool,
) -> Result<ScanResult, String> {
    let started = Instant::now();
    let mut tree = Some(TreeBuilder::new(root.to_path_buf()));
    let mut pool = None;
    let mut trees: Option<Vec<TreeBuilder>> = None;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut last_report = Instant::now();
    let mut warmup = Vec::with_capacity(WALK_PARALLEL_THRESHOLD as usize);
    let mut batch = Vec::with_capacity(WALK_BATCH_SIZE);

    for entry in WalkDir::new(root).follow_links(false) {
        if cancel.load(Ordering::Relaxed) {
            return Err("Сканирование остановлено".to_owned());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.path() == root {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
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
        if let Some(trees) = trees.as_mut() {
            batch.push(record);
            if batch.len() >= WALK_BATCH_SIZE {
                insert_records(
                    std::mem::replace(&mut batch, Vec::with_capacity(WALK_BATCH_SIZE)),
                    root,
                    pool.as_ref().unwrap(),
                    trees,
                );
            }
        } else {
            warmup.push(record);
            if warmup.len() >= WALK_PARALLEL_THRESHOLD as usize {
                let worker_pool = build_pool().map_err(|error| error.to_string())?;
                let worker_count = worker_pool.current_num_threads();
                let mut shards: Vec<_> = (0..worker_count)
                    .map(|_| TreeBuilder::new(root.to_path_buf()))
                    .collect();
                tree = None;
                insert_records(std::mem::take(&mut warmup), root, &worker_pool, &mut shards);
                pool = Some(worker_pool);
                trees = Some(shards);
            }
        }
        if last_report.elapsed() >= Duration::from_millis(120) {
            let _ = tx.send(ScanEvent::Progress {
                files,
                total: None,
                bytes,
                phase: "Сканирую файлы".to_owned(),
            });
            last_report = Instant::now();
        }
    }
    let nodes = if let (Some(pool), Some(mut trees)) = (pool, trees) {
        insert_records(batch, root, &pool, &mut trees);
        let shards = pool.install(|| trees.into_par_iter().map(TreeBuilder::finish).collect());
        TreeBuilder::merge_finished_shards(shards)
    } else {
        let mut tree = tree.unwrap();
        for record in warmup {
            tree.add(
                record.path,
                record.is_dir,
                record.size,
                record.modified,
                record.attributes,
            );
        }
        tree.finish()
    };

    let _ = tx.send(ScanEvent::Progress {
        files,
        total: Some(files),
        bytes,
        phase: "Building directory totals and ordering".to_owned(),
    });
    Ok(ScanResult {
        nodes: Arc::new(nodes),
        mode: ScanMode::DirectoryWalk,
        files,
        elapsed: started.elapsed(),
    })
}

fn insert_records(
    records: Vec<ScanEntry>,
    root: &Path,
    pool: &rayon::ThreadPool,
    trees: &mut [TreeBuilder],
) {
    let mut shards: Vec<Vec<ScanEntry>> = (0..trees.len()).map(|_| Vec::new()).collect();
    for record in records {
        let shard = shard_for(&record.path, root, trees.len());
        shards[shard].push(record);
    }
    pool.install(|| {
        shards
            .into_par_iter()
            .zip(trees.par_iter_mut())
            .for_each(|(records, tree)| {
                for record in records {
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
        .map(|component| component.as_os_str().to_string_lossy())
        .unwrap_or_else(|| path.as_os_str().to_string_lossy());
    let hash = first_component
        .bytes()
        .fold(2_166_136_261_u32, |hash, byte| {
            hash.wrapping_mul(16_777_619) ^ byte.to_ascii_lowercase() as u32
        });
    hash as usize % shard_count
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
        let records: Vec<_> = (0..40)
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
        insert_records(records, root, &pool, &mut trees);
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

#[cfg(windows)]
fn scan_mft(
    root: &Path,
    tx: &Sender<ScanEvent>,
    cancel: &AtomicBool,
) -> Result<ScanResult, String> {
    use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, Volume};

    let started = Instant::now();
    let drive = root
        .to_string_lossy()
        .chars()
        .next()
        .ok_or_else(|| "Не удалось определить букву диска".to_owned())?;
    let volume_path = format!(r"\\.\{}:", drive);
    let _ = tx.send(ScanEvent::Progress {
        files: 0,
        total: None,
        bytes: 0,
        phase: "Подготавливаю чтение MFT; требуются права администратора".to_owned(),
    });
    let volume = Volume::new(volume_path.as_str()).map_err(|error| error.to_string())?;
    let mut scan = MftScan::new(volume).map_err(|error| error.to_string())?;
    let total = Some(scan.record_count());
    let mut cache = DefaultPathCache::new();
    let pool = build_pool().map_err(|error| error.to_string())?;
    let worker_count = pool.current_num_threads();
    let mut trees: Vec<_> = (0..worker_count)
        .map(|_| TreeBuilder::new(root.to_path_buf()))
        .collect();
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut last_report = Instant::now();

    while let Some(chunk) = scan.next_chunk().map_err(|error| error.to_string())? {
        if cancel.load(Ordering::Relaxed) {
            return Err("Сканирование остановлено".to_owned());
        }
        let mut shards: Vec<Vec<ScanEntry>> = (0..worker_count).map(|_| Vec::new()).collect();
        for file in chunk.files() {
            let info = FileInfo::with_cache(&file, &mut cache);
            if let Some(path) = info.path {
                let path_text = path.to_string_lossy();
                if let Some(path_text) = path_text.strip_prefix(r"\\.\") {
                    let path = PathBuf::from(path_text);
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
                }
            }
            if last_report.elapsed() >= Duration::from_millis(120) {
                let _ = tx.send(ScanEvent::Progress {
                    files,
                    total,
                    bytes,
                    phase: "Читаю MFT".to_owned(),
                });
                last_report = Instant::now();
            }
        }
        pool.install(|| {
            shards
                .into_par_iter()
                .zip(trees.par_iter_mut())
                .for_each(|(records, tree)| {
                    for record in records {
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
    }

    let _ = tx.send(ScanEvent::Progress {
        files,
        total,
        bytes,
        phase: "Building directory totals and ordering".to_owned(),
    });
    let shards = pool.install(|| trees.into_par_iter().map(TreeBuilder::finish).collect());
    Ok(ScanResult {
        nodes: Arc::new(TreeBuilder::merge_finished_shards(shards)),
        mode: ScanMode::NtfsMft,
        files,
        elapsed: started.elapsed(),
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
        return metadata.file_attributes();
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        0
    }
}

#[cfg(windows)]
fn is_drive_root(path: &Path) -> bool {
    let value = path.to_string_lossy();
    let bytes = value.as_bytes();
    bytes.len() == 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}

#[cfg(windows)]
fn is_access_denied(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("access is denied")
        || lower.contains("access denied")
        || lower.contains("os error 5")
}

#[cfg(windows)]
pub fn relaunch_elevated(root: &Path) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> *mut c_void;
    }

    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe_wide: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let operation: Vec<u16> = "runas".encode_utf16().chain(Some(0)).collect();
    let root_text = root.display().to_string();
    if root_text.contains('"') {
        return Err("Path contains an unsupported quote character".to_owned());
    }
    let quoted_root = if root_text.ends_with('\\') {
        format!("{root_text}\\")
    } else {
        root_text.clone()
    };
    let parameters = format!("--scan \"{quoted_root}\"");
    let parameters_wide: Vec<u16> = parameters.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            exe_wide.as_ptr(),
            parameters_wide.as_ptr(),
            std::ptr::null(),
            1,
        )
    } as isize;
    if result <= 32 {
        Err(format!(
            "Could not restart as administrator (code {result})"
        ))
    } else {
        Ok(())
    }
}
