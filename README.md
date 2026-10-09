# Compactor

Compactor is a small Windows GUI for applying Windows Overlay Filter (WOF) filesystem compression to folders.

This repository is a maintained fork of [Freaky/Compactor](https://github.com/Freaky/Compactor). It retains the original Rust GUI and WOF-based compression model while fixing known issues and improving analysis and maintenance.

![Compactor](docs/compactor.webp)

## Features

- XPRESS4K, XPRESS8K, XPRESS16K, and LZX compression
- LZX as the default compression mode
- Fast sampled analysis or optional Thorough analysis for maximum coverage
- Configurable minimum measured savings to retain compression, defaulting to 1% and at least one NTFS cluster
- Clearly labelled generic sample estimates, followed by measurements from the protected file handle
- Storage-aware multithreaded analysis, compression, and decompression on SSDs
- Conservative physical-core-aware LZX Auto concurrency with manual override
- Single-threaded HDD operation by default
- HDD analysis sampling designed to reduce seek overhead
- Configurable compression/decompression worker priority, defaulting to Below Normal
- DirectStorage game-root detection and protection, enabled by default with a Settings override
- Decompress + exclude for restoring a folder and preventing future recompression
- WOF availability checks before file transformations begin
- The system is kept awake during compression/decompression while the display may still turn off
- Local-NTFS target validation and protection for Windows-managed paths
- In-app searchable viewer for WOF-compressed files and the folders containing them
- Pause, resume, and stop controls
- Timestamp preservation after compression and decompression
- Automatic retry of previously incompressible files after they change
- Case-insensitive exclusion patterns with blank-entry filtering
- Native WOF detection, with protection for other reparse points, hardlinks, encrypted, offline, sparse and NTFS-compressed files
- Persistent operation reports with separate success, low-benefit, skipped and error counts
- Reanalysis after settings changes, safe shutdown while paused, and atomic configuration writes
- No default file-extension exclusions

## Defaults

- Compression: `LZX`
- Analysis: `Sampled` (choose `Thorough` for maximum coverage)
- Minimum measured savings: `1%` and at least one NTFS cluster
- Maximum threads: `Auto`
- Compression priority: `Below Normal`
- HDDs only use 1 thread: enabled
- Protect DirectStorage games: enabled
- Excluded paths:
  - `*:\Windows*`
  - `*:\System Volume Information*`
  - `*:\$*`

`Auto` is storage- and workload-aware. On SSDs it uses up to eight workers for analysis. XPRESS compression can use up to 16 logical-CPU workers. LZX Auto uses one outer worker on CPUs with four or fewer physical cores and at most two on larger CPUs because LZX WOF operations can be CPU-intensive. A manual limit from 1 to 16 is treated as an explicit override and is respected for LZX as well. Auto decompression uses storage-aware parallelism up to eight workers. HDDs still use one worker by default when the HDD safeguard is enabled, and unknown storage is handled conservatively with one worker. In Sampled mode, compression reuses a valid analysis estimate when the file has not changed, avoiding duplicate sampling before WOF compression.

Compression/decompression worker priority can be set to Lowest, Below Normal, Normal, Above Normal, or Highest. Only the worker threads are affected; the GUI and analysis threads remain at normal priority. Below Normal is the default so idle CPU capacity can still be used while foreground work gets preference when the system is busy.

File extensions are not used to decide whether a file should be compressed. In **Sampled** mode, a fast generic estimator chooses candidates; it can miss files that the selected WOF algorithm would compress. In **Thorough** mode, the estimate is advisory and every otherwise eligible, uncached file is tried with the selected algorithm. Both modes retain compression only when the measured allocation reduction meets the percentage threshold and saves at least one NTFS cluster. A result below that threshold is expanded again; any failure to restore is reported explicitly.

For the greatest coverage, choose **LZX + Thorough**. LZX is the most compact algorithm listed by Windows for this compression model; Thorough changes which files are attempted, not the format. It does not promise a fixed improvement and can cost substantially more CPU time and writes, particularly on already compressed archives or media. **LZX + Sampled** remains the default to preserve the existing performance tradeoff. See [compression policy and limitations](docs/compression-policy.md).

## Installation

Download the latest Windows x64 executable from the [Releases](https://github.com/wefalltomorrow/Compactor/releases) page.

Compactor is portable and does not require an installer or background service.

## Usage

1. Choose a folder.
2. Wait for analysis to complete.
3. Review current disk usage and, in Sampled mode, the generic savings estimate.
4. Choose the algorithm, analysis mode, savings threshold, thread limit, worker priority and DirectStorage protection in Settings. Analyse again after saving changes.
5. Select Compress, then review the persistent operation report. The final savings count uses measurements taken around successful file operations, not the earlier analysis snapshot.

Use Decompress to remove supported WOF file backing, including files compressed by another WOF tool. The operation needs free space to materialise the contents and reports any file that cannot be expanded. After decompression, Analyse again before compressing. Use **Decompress + exclude** when you also want the selected folder added to File exclusions after all remaining WOF files have been successfully expanded. Literal folder names containing brackets or braces are handled correctly. After analysis, select View beside the compressed count to browse WOF-compressed files or their containing folders inside Compactor. The viewer includes a Files/Folders selector, path filtering and pagination for large scans.

## Notes

Compactor is best suited to applications and game files that change infrequently. Modifying a WOF-compressed file causes Windows to materialise it again, so updated folders may need to be analysed and compressed again.

Compression and decompression hold each active file against concurrent content writers and deleters while still allowing readers. Attributes, backing format, hardlinks and final path are checked again after opening. A file redirected outside the scan's canonical root is refused. A mutex keyed by file identity prevents cooperating Compactor instances from transforming the same file concurrently. This does not coordinate with unrelated compression tools.

In Sampled mode, files that change between analysis and compression are re-estimated. Cache entries are tied to the fingerprint observed under the protected handle, algorithm, analysis mode, savings threshold and cluster size; sampled rejections and I/O errors are not recorded as measured compression failures.

Before a compression run, Compactor searches independently of compression exclusions for `dstorage.dll` or `dstoragecore.dll`. When these are found, Compactor identifies the likely game/install root using common game-library layouts and Unreal Engine DirectStorage layouts. With **Protect DirectStorage games** enabled, files under those detected roots are removed from the compression queue before WOF is applied. The setting can be disabled when you deliberately want to compress such files. Detection is conservative and based on the presence/layout of the DirectStorage runtime; it cannot prove that every detected game is actively using BypassIO or find every possible runtime layout. The preflight is cancellable and stops compression if it cannot complete the traversal.

Before compression or decompression starts, Compactor also checks that Windows reports WOF support and probes the target volume using available files. If WOF is explicitly unavailable, the operation is stopped with a clear error instead of failing repeatedly file by file.

Do not run Compactor blindly across an entire system drive. Whole-drive roots, network/UNC targets, non-NTFS filesystems, the active Windows directory, `System Volume Information`, root `$*` directories, and the root `Recovery` directory are rejected. Folder aliases through junctions, symbolic links or mount points are rejected; select the actual local NTFS folder. User-selected folders can still contain databases, virtual machines, active logs, or other write-heavy files that are poor candidates for WOF compression.

Compactor does not elevate itself through UAC. Protected files may require running the program with appropriate permissions.

Keep backups of important data. The software is provided without warranty under the MIT License.

## Changes from upstream v0.10.1

This fork includes the following changes:

- Correct Win32 `BOOL` handling for WOF `DeviceIoControl` calls
- Correct handling of `ERROR_COMPRESSION_NOT_BENEFICIAL`
- Required read-data/write-attributes access for WOF operations
- File handles that block concurrent writers/deleters during active WOF operations
- Case-insensitive exclusions and blank-entry filtering
- Skipping of encrypted, offline and legacy NTFS-compressed files, with explicit WOF-aware handling of sparse/reparse metadata
- Incompressible-cache keys scoped to the file fingerprint and selected compression policy
- Zero-safe progress calculations and saturating size arithmetic
- Byte-aware compression progress
- Configurable savings thresholds
- Estimated post-compression size and savings in the GUI
- Actual NTFS cluster-size-aware eligibility and projected allocation
- LZX as the default compression mode with a conservative one-or-two-worker Auto policy and manual override
- Configurable compression/decompression worker priority, defaulting to Below Normal
- Storage-aware parallel decompression with the HDD single-worker safeguard
- Decompress + exclude workflow
- System-sleep prevention during compression/decompression without forcing the display awake
- Stronger local-NTFS and Windows-managed-path target validation
- DirectStorage game-root detection and default protection with an override
- WOF availability preflight checks
- Coalesced high-frequency GUI updates for smoother large operations
- Removal of default file-extension exclusions
- Storage-aware worker selection and parallel SSD processing
- Reduced-seek HDD analysis sampling
- Lower scan memory usage for large excluded trees and bounded parallel estimation batches
- More accurate post-decompression on-disk accounting
- In-app compressed file/folder viewer with filtering and pagination
- Windows x64 CI on stable Rust

See [CHANGELOG.md](CHANGELOG.md) for the full change history.

## Building

Requirements:

- Windows 10 or Windows 11
- Stable Rust with the `x86_64-pc-windows-msvc` toolchain
- Microsoft C++ build tools required by the Rust MSVC toolchain

Build with:

```powershell
cargo build --release
```

The executable is written to:

```text
target\release\Compactor.exe
```

## Technical details

Compactor is primarily written in [Rust](https://www.rust-lang.org/) and uses a local embedded web-view for the GUI. It does not require remote web resources at runtime.

WOF compression is applied through [`FSCTL_SET_EXTERNAL_BACKING`](https://learn.microsoft.com/windows-hardware/drivers/ifs/fsctl-set-external-backing) and removed through [`FSCTL_DELETE_EXTERNAL_BACKING`](https://learn.microsoft.com/windows-hardware/drivers/ifs/fsctl-delete-external-backing).

Compressibility sampling uses Thomas Hurst's [compresstimator](https://github.com/Freaky/compresstimator) project. SSD analysis uses distributed sampling, while HDD analysis uses a small number of contiguous sample windows to reduce seek overhead. Estimates are rounded to the volume's NTFS cluster size. They are generic sampling results, not measurements of the selected WOF codec; uncalibrated per-codec multipliers have been removed. Thorough mode does not present that estimate as its final savings prediction.

WOF state is queried explicitly. Measurements use `GetFileInformationByHandleEx(FileCompressionInfo)` through a protected file identity and an identity-checked normal WOF view. Unsupported backing formats are left unchanged; malformed API responses and ambiguous allocations are reported as errors. These cases are never counted as successful compression. Existing WOF files are preserved by Compress; the program does not automatically cycle through algorithms or expand an existing compressed installation to try LZX.

See [testing and Windows verification](docs/TESTING.md) for reproducible checks, including real WOF round trips on isolated NTFS fixtures.

Windows storage-property queries are used to distinguish storage with and without seek penalties. If storage type cannot be determined, Compactor uses one worker thread. Windows processor-topology information is used to choose the conservative LZX Auto worker count; explicit manual thread limits override that Auto policy.

## Credits

Compactor was originally written by [Thomas Hurst (Freaky)](https://github.com/Freaky). This fork retains the original MIT-licensed work and attribution.

See [LICENSE.txt](LICENSE.txt) for license terms.
