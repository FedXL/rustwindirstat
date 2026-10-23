# Windirstat RS 🦀

Chat GPT please rewrite slow WINDIRSTAT to RUST 🦀

Fast Windows disk usage analyzer written in Rust.

## Release 1.00

[Download Windirstat RS 1.00 for Windows](dist/windirstat-rs-1.00.exe)

## Features

- Reads NTFS metadata through the Master File Table when scanning a drive root from an elevated process.
- Falls back to a regular directory walk when MFT access is unavailable or when a folder is scanned.
- Shows a directory tree, an interactive size treemap, largest search matches, and a summary by file extension.
- Offers Explorer and delete actions for the selected file or directory. Deletion requires a confirmation dialog.
- Keeps the window responsive while the scan runs in a background thread.

## Run

Install the stable Rust toolchain, then from this directory run:

```powershell
cargo run --release
```

To scan another drive or folder, type its path in the Path field. For the fast NTFS mode, run the program as Administrator and enter a drive root such as `C:\`. Folder scans use the regular walker.

The fast MFT scan reads NTFS metadata and does not read file contents. Size totals are based on each file's default data stream; hard-linked paths may be counted separately.
