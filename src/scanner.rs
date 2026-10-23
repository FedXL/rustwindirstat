use crate::model::{Node, TreeBuilder};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

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
    let mut tree = TreeBuilder::new(root.to_path_buf());
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut last_report = Instant::now();

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
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let size = if is_dir { 0 } else { metadata.len() };
        let modified = metadata.modified().ok();
        let attributes = file_attributes(&metadata);
        tree.add(
            entry.path().to_path_buf(),
            is_dir,
            size,
            modified,
            attributes,
        );
        if !is_dir {
            files += 1;
            bytes = bytes.saturating_add(size);
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

    let _ = tx.send(ScanEvent::Progress {
        files,
        total: Some(files),
        bytes,
        phase: "Building directory totals and ordering".to_owned(),
    });
    Ok(ScanResult {
        nodes: Arc::new(tree.finish()),
        mode: ScanMode::DirectoryWalk,
        files,
        elapsed: started.elapsed(),
    })
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
    let mut tree = TreeBuilder::new(root.to_path_buf());
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut last_report = Instant::now();

    while let Some(chunk) = scan.next_chunk().map_err(|error| error.to_string())? {
        if cancel.load(Ordering::Relaxed) {
            return Err("Сканирование остановлено".to_owned());
        }
        for file in chunk.files() {
            let info = FileInfo::with_cache(&file, &mut cache);
            if let Some(path) = info.path {
                if path.to_string_lossy().starts_with(r"\\.\") {
                    let path = PathBuf::from(
                        path.to_string_lossy()
                            .trim_start_matches(r"\\.\")
                            .to_owned(),
                    );
                    let modified = info.modified.and_then(system_time_from_unix);
                    tree.add(
                        path,
                        info.is_directory,
                        info.size,
                        modified,
                        info.file_attributes,
                    );
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
    }

    let _ = tx.send(ScanEvent::Progress {
        files,
        total,
        bytes,
        phase: "Building directory totals and ordering".to_owned(),
    });
    Ok(ScanResult {
        nodes: Arc::new(tree.finish()),
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
