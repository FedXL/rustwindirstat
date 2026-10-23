# Responsiveness refactor

Development branch: `refactor/responsive-core`. The published 1.01 binary is the previous release.

- Only one scan is active. Cancelling a scan does not start the fallback scanner. Search results are invalidated whenever the scan snapshot changes.
- Directory rows are cached until the snapshot, expansion, or sorting changes. Names use one normalized sort key per entry during sorting.
- Selecting a tile centers its directory row using the same row spacing as the virtualized list, including for rows far outside the previous viewport.
- Search runs on one background worker with cancellation, a 150 ms debounce, and a bounded heap of 500 largest matches. ASCII searches do not allocate a lowercase full path per entry.
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

The MFT implementation still uses `ntfs-reader`'s two-pass initialization. Its initial synchronous pass has no cancellation hook; cancellation is checked immediately after that call returns. Phase timings distinguish initialization from subsequent decoding and tree construction. A compact tree based on NTFS file references is a separate change requiring whole-drive benchmarks and parity checks for stale parent references and hard links.
