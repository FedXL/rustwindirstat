# Responsiveness refactor

Development branch: `refactor/mft-index` (includes `refactor/responsive-core`). The published 1.01 binary is the previous release.

- Only one scan is active. Cancelling a scan does not start the fallback scanner. Search results are invalidated whenever the scan snapshot changes.
- Directory rows are cached until the snapshot, expansion, or sorting changes. Names use one normalized sort key per entry during sorting.
- Selecting a tile centers its directory row using the same row spacing as the virtualized list, including for rows far outside the previous viewport.
- Search runs on one background worker with cancellation, a 150 ms debounce, and a bounded heap of 500 largest matches. ASCII paths and queries use a comparison without lowercase string allocations; Unicode searches preserve the existing lowercase semantics.
- Treemap workers report cancellation and failures separately. Terminal failures do not automatically restart the same render. Layout uses pixel-area aspect ratios; serial and parallel rendering share the pixel kernel.
- The application reuses one bounded Rayon pool. Scan batches reuse staging buffers. Directory lookup keys preserve native Windows names and case-sensitive entries.
- Deletion runs in a background job. Windows shell actions preserve native paths. Fonts are loaded from the Windows directory at runtime.
- Scan results expose skipped entries and phase timings in the status tooltip. MFT progress counts processed records separately from files.

## Measurements

A local in-memory fixture with 100,000 direct children took about 382 ms per name-sort rebuild before the change. The new initial name sort took about 54 ms; unchanged frames reuse the rows without sorting. These are synthetic timings on the development PC, not a whole-drive scan benchmark.

## Checks

```powershell
cargo test --release
cargo clippy --release --all-targets -- -D warnings
cargo test --release benchmark_cached_directory_rows -- --ignored --nocapture
```

## Compact MFT index

The default MFT backend decodes metadata in work-stealing batches of 256 records. It links nodes by full NTFS reference, including the sequence number. Reused/missing parents and unreachable cycles are excluded and reported. Reserved directories are retained only when normal entries need them. The previous full-path backend remains available for diagnostic comparisons.

Snapshots store native names, compact parent references, and adjacency lists. They do not retain full paths. Explorer/delete actions reconstruct the selected path; search traverses with one reusable path buffer. The fixed node structure is 80 bytes on the Windows x64 build, versus 144 bytes before this change. Fallback tree construction also uses parent/name keys rather than full-path keys.

The local `ntfs-reader` copy provides cancellable/progress-aware initialization and a live-metadata mode that omits unused directory-path indexes and deleted extension records. Its upstream license files and local-change notes are in `vendor/ntfs-reader/`. Cancellation is cooperative between batches; an in-progress device read must return first.

The default builder checks an estimated 768 MiB model/workspace budget and the process's committed private memory between read batches. The path-based diagnostic backend has a separate 3 GiB process threshold so it can finish comparisons on the development PC. These checks are cooperative thresholds rather than an OS-enforced hard allocation limit.

## Whole-drive diagnostics

Run from an elevated process; these commands read metadata and write diagnostic results, without modifying scanned files:

```powershell
windirstat-rs.exe --benchmark C:\ --backend references --output references.json
windirstat-rs.exe --benchmark C:\ --backend paths --output paths.json
windirstat-rs.exe --benchmark-suite C:\ --output-dir benchmark-results
```

Each run reports counts, a commutative path/size digest, phase timings, snapshot bytes, process memory peaks, and decoded records per Rayon worker. The suite uses separate processes in an ABBA order and includes an initialization-cancellation check. Each child has a 60-second timeout. A live disk changes between runs, so identical digests are not guaranteed; synthetic metadata tests provide deterministic parity checks.

The first successful references run on C: took 9.23 seconds for 2,621,007 nodes. Its retained snapshot was 287,189,904 bytes and its process peak commit was 603,705,344 bytes. This is a local measurement, not a hardware-independent performance guarantee.

Regression coverage includes synthetic NTFS bytes with extension records, multiple names/hard links, alternate streams, stale parent sequences, invalid path components, cycles, a 10,000-directory chain, and an actual egui frame test that reveals a distant selected file and renders 100 frames. The upstream scan-specific library tests are also run separately.
