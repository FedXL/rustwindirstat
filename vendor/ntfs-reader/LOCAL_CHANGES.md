# Local changes to ntfs-reader 0.6.0

Upstream: https://github.com/kikijiki/ntfs-reader

The upstream MIT/Apache-2.0 license files and notices are retained.

- Added `NtfsReaderError::Cancelled` and `MftScan::with_chunk_records_controlled` to report first-pass progress and cancel cooperatively between batches and during directory-name settling. A device read already in progress must return before cancellation can be observed.
- Added `with_live_metadata_controlled` for the disk analyzer's file-reference index. It retains root/live extension records, omits the unused directory-path index and deleted extension records, and deliberately does not provide full-path/deleted-stream resolution. The existing constructors retain their original behavior.
- Added synthetic constructors behind the existing `internals` feature so application tests can verify parsing, extension records, reference reuse, and cancellation without accessing a real drive.
- Added `NtfsAttribute::is_unnamed` to distinguish unnamed streams from corrupt named streams without allocating an attribute-name string.

Application parity tests compare metadata-only scans against `FileInfo::with_cache` and the full-path constructor using synthetic MFT bytes.
