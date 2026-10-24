use crate::scanner::{self, MftBackend, ScanError, ScanEvent, ScanMode};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn argument(args: &[OsString], flag: &str) -> Option<OsString> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

pub fn run(args: &[OsString]) -> i32 {
    let root = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\"));
    let output = argument(args, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("benchmark.json"));
    let backend = if argument(args, "--backend").as_deref() == Some(std::ffi::OsStr::new("paths")) {
        MftBackend::Paths
    } else {
        MftBackend::References
    };
    let timeout = argument(args, "--cancel-after-ms")
        .and_then(|value| value.to_str()?.parse::<u64>().ok())
        .unwrap_or(60_000);
    let cancel = Arc::new(AtomicBool::new(false));
    let timer_cancel = Arc::clone(&cancel);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(timeout));
        timer_cancel.store(true, Ordering::Relaxed);
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let observer_cancel = Arc::clone(&cancel);
    let status_file = output.with_extension("progress.txt");
    let observer = std::thread::spawn(move || {
        let mut fallback = None;
        for event in rx {
            match event {
                ScanEvent::Progress {
                    processed,
                    total,
                    phase,
                    ..
                } => {
                    let _ = std::fs::write(
                        &status_file,
                        format!("{} {processed}/{}", phase.label(), total.unwrap_or(0)),
                    );
                }
                ScanEvent::MftFallback { reason, .. } => {
                    // A benchmark of MFT must never quietly turn into a whole-drive walk.
                    fallback = Some(reason);
                    observer_cancel.store(true, Ordering::Relaxed);
                }
                ScanEvent::Finished(_) => {}
            }
        }
        fallback
    });
    let started = Instant::now();
    let result = scanner::scan_with_backend(&root, &tx, &cancel, backend);
    drop(tx);
    let fallback = observer.join().ok().flatten();
    let elapsed = started.elapsed().as_secs_f64();
    let memory = crate::metrics::process_memory();
    let (report, exit_code) = match result {
        Ok(result) => {
            let mut digest = 0u64;
            let mut path_bytes = 0usize;
            result.nodes.visit_paths(|index, path| {
                let mut hash = 0xcbf29ce484222325u64;
                for byte in path
                    .as_os_str()
                    .as_encoded_bytes()
                    .iter()
                    .copied()
                    .chain(result.nodes[index].size.to_le_bytes())
                    .chain([u8::from(result.nodes[index].is_dir)])
                {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
                digest = digest.wrapping_add(hash);
                path_bytes += path.as_os_str().as_encoded_bytes().len();
                std::ops::ControlFlow::Continue(())
            });
            let ok = result.mode == ScanMode::NtfsMft && fallback.is_none();
            (
                format!(
                    "{{\"ok\":{ok},\"backend\":{},\"root\":{},\"elapsed_seconds\":{elapsed:.6},\"nodes\":{},\"files\":{},\"bytes\":{},\"skipped\":{},\"corrupt_records\":{},\"snapshot_bytes\":{},\"node_fixed_bytes\":{},\"avoided_path_payload_bytes\":{path_bytes},\"peak_private_bytes\":{},\"peak_working_set_bytes\":{},\"digest\":\"{digest:016x}\",\"timings\":{}}}",
                    json_string(&format!("{backend:?}")),
                    json_string(&result.nodes.root().display().to_string()),
                    result.nodes.len(),
                    result.files,
                    result.nodes[0].size,
                    result.warnings.skipped,
                    result.warnings.corrupt_records,
                    result.nodes.size_in_memory(),
                    std::mem::size_of::<crate::model::Node>(),
                    memory.peak_private_bytes,
                    memory.peak_working_set_bytes,
                    result.timings.json()
                ),
                if ok { 0 } else { 2 },
            )
        }
        Err(error) => {
            let cancelled = matches!(error, ScanError::Cancelled) && fallback.is_none();
            (
                format!(
                    "{{\"ok\":false,\"cancelled\":{cancelled},\"backend\":{},\"elapsed_seconds\":{elapsed:.6},\"peak_private_bytes\":{},\"error\":{}}}",
                    json_string(&format!("{backend:?}")),
                    memory.peak_private_bytes,
                    json_string(fallback.as_deref().unwrap_or(&error.to_string()))
                ),
                if cancelled && timeout < 60_000 { 0 } else { 2 },
            )
        }
    };
    match std::fs::write(&output, report) {
        Ok(()) => exit_code,
        Err(error) => {
            eprintln!("{}: {error}", output.display());
            2
        }
    }
}

pub fn suite(args: &[OsString]) -> i32 {
    let root = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\"));
    let directory = argument(args, "--output-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("benchmark-results"));
    if let Err(error) = std::fs::create_dir_all(&directory) {
        eprintln!("{error}");
        return 2;
    }
    let Ok(exe) = std::env::current_exe() else {
        return 2;
    };
    for (name, backend, timeout) in [
        ("cancel-initialization", "references", "100"),
        ("paths-1", "paths", "60000"),
        ("references-1", "references", "60000"),
        ("references-2", "references", "60000"),
        ("paths-2", "paths", "60000"),
    ] {
        if directory.join("stop").exists() {
            return 2;
        }
        let _ = std::fs::write(directory.join("suite-status.txt"), name);
        let status = std::process::Command::new(&exe)
            .arg("--benchmark")
            .arg(&root)
            .args([
                "--backend",
                backend,
                "--cancel-after-ms",
                timeout,
                "--output",
            ])
            .arg(directory.join(format!("{name}.json")))
            .status();
        if !status.is_ok_and(|status| status.success()) {
            let _ = std::fs::write(
                directory.join("suite-status.txt"),
                format!("failed: {name}"),
            );
            return 2;
        }
    }
    let _ = std::fs::write(directory.join("suite-status.txt"), "complete");
    0
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch < '\u{20}' => {
                use std::fmt::Write;
                let _ = write!(output, "\\u{:04x}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}
