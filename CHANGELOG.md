# Changelog

## Unreleased

### Added

- Optional Thorough analysis, which tries the selected WOF algorithm on every eligible, uncached file instead of rejecting candidates through a generic sample. Existing settings continue to use Sampled mode; LZX remains the default algorithm.
- A measured retention policy: require the configured percentage reduction and at least one NTFS cluster of savings, otherwise restore the newly compressed file. Report failed measurements and restoration failures explicitly.
- File-identity-aware WOF inspection, guarded normal-view allocation queries, canonical-root containment checks, hardlink protection and cooperative per-file locking between Compactor instances.
- Persistent operation reports, bounded error details, behaviour tests for the embedded UI and explicit native Windows/NTFS integration tests.
- Support for checking Windows sources with a real MinGW cross-toolchain, without bypassing resource compilation.

### Fixed

- Recognise actual WOF backing before rejecting sparse/reparse metadata. Do not infer the compression format from a size comparison.
- Validate Windows API response lengths, versions, providers and algorithm identifiers; accept the existing XPRESS4K setting consistently.
- Scope the incompressible cache to file fingerprint, algorithm, mode, threshold and cluster size; cache only measured negative results from the protected identity.
- Preserve pending cache entries after write/flush/sync failures and recover from truncated or missing cache files.
- Stop scheduling work when the GUI closes, including while paused. Prioritise cancellation over an already-ready analysis/preflight result.
- Process each original decompression candidate at most once per run, retaining failed WOF files for inspection and a later retry.
- Invalidate stale analyses after settings changes and require new analysis after decompression or errors.
- Detect DirectStorage signals independently of compression exclusions, support cancelling the preflight and refuse to compress after an incomplete check.
- Save configuration atomically and validate it on load. Preserve invalid configuration bytes until the user saves a corrected configuration.
- Escape automatically added folder exclusions, reject invalid numeric settings, reset empty charts and refresh displayed units and file/folder views correctly.

### Changed

- Update `crossbeam-channel` from 0.5.0 to 0.5.17 and its `crossbeam-utils` dependency from 0.8.1 to 0.8.23, with the resolved versions recorded in Cargo.lock.
- Removed uncalibrated algorithm-specific estimate multipliers; estimates are explicitly generic and provisional.
- Native WOF tests use temporary fixtures instead of modifying Cargo.lock or scanning a user's C:\Games directory.
- CI and release workflows run interface tests, workspace tests and explicit WOF round-trip checks before publishing artifacts.

## [0.11.5] - 2026-09-14

### Added

- Protect detected DirectStorage game roots from new WOF compression by default, with a Settings toggle to allow an explicit override.
- Infer DirectStorage game roots from common launcher/library layouts and Unreal Engine DirectStorage layouts instead of treating an entire game library as one target.
- Add **Decompress + exclude** to expand a selected folder and persist it in File exclusions so it is not compressed again later.
- Check Windows/WOF availability before starting compression or decompression and show a clear error when WOF is unavailable on the target volume.

### Changed

- Decompression now uses storage-aware parallel workers on SSDs. Auto is capped at eight workers, manual thread limits are respected, and the HDD single-worker safeguard still applies.
- Coalesce high-frequency status and folder-summary updates in the embedded UI so large scans and file operations do less redundant DOM work.
- DirectStorage detection now acts as a protection mechanism rather than only displaying a warning.

### Fixed

- Failed decompression jobs remain classified as compressed instead of being moved into the skipped bucket.
- **Decompress + exclude** immediately updates the analysed folder state so the newly excluded files are not offered for recompression in the same session.

## [0.11.4] - 2026-09-08

### Changed

- Rework the Settings page into clear Exclusions, Compression, and Display panels.
- Use a responsive two-column settings layout that collapses to one column on narrower windows.
- Keep labels, controls, and help text aligned, with shorter worker/thread guidance beneath the relevant controls.

### Fixed

- Prevent Settings controls and help text from running into or overlapping each other at common window sizes and display scaling levels.

## [0.11.3] - 2026-09-08

### Added

- Warn when a selected folder contains the DirectStorage runtime (`dstorage.dll` or `dstoragecore.dll`).
- Keep the system awake during compression and decompression without forcing the display to remain on.
- Detect the target NTFS cluster size for compression eligibility and projected on-disk allocation.
- Add configurable compression/decompression worker priority: Lowest, Below Normal, Normal, Above Normal, or Highest. Below Normal is the default.

### Changed

- LZX Auto concurrency is deliberately conservative: one worker on CPUs with four or fewer physical cores and at most two workers on larger CPUs. An explicit manual thread limit overrides the LZX Auto policy; the HDD single-thread safeguard still applies when enabled.
- Worker priority applies only to compression/decompression threads; the GUI and analysis threads remain at normal priority.
- Analysis uses bounded 4096-file estimation batches instead of allowing the complete candidate set to accumulate before parallel estimation.
- Skipped files retain counts and size totals without retaining every skipped path, reducing memory use on very large scans.
- Displayed projected savings are adjusted for the selected WOF algorithm and rounded to the target volume's cluster size while the eligibility decision continues to use the raw sampled compressibility result.
- Decompression results use the file's actual post-operation on-disk allocation instead of assuming physical size equals logical size.

### Fixed

- Compression and decompression now open each active file while allowing readers but denying concurrent writers and deleters, reducing the risk of racing game/application updates during a WOF operation.
- File size and modification time are now read from the already-open protected handle before deciding whether a previous analysis estimate is still reusable, closing the remaining change-between-check-and-open race.
- Correct logical-versus-physical file accounting so sparse/allocation edge cases do not distort folder totals or estimated savings.
- Skip files whose logical length cannot save an NTFS cluster instead of spending analysis time on files that cannot reduce allocation.
- Strengthen target validation: only local NTFS folders are accepted; whole-drive roots, network/UNC paths, the active Windows directory, `System Volume Information`, root `$*` directories, root `Recovery`, and legacy LZNT1-compressed target folders are rejected.
- Correct the GUI allocation breakdown to use actual on-disk bytes for physical categories.
- Repair the v0.11.3 lockfile so existing dependency checksums remain identical to the known-good v0.11.2 lockfile; only the local package version changes.

## [0.11.2] - 2026-09-07

### Added

- In-app viewer for folders containing WOF-compressed files, with path filtering and pagination for large scans.

### Changed

- Auto threading now uses up to eight analysis workers and up to 16 compression workers on SSDs while retaining the single-thread HDD safeguard.
- Compression reuses the analysis compressibility estimate when file size and modification time are unchanged, avoiding duplicate sampling; changed files are re-estimated before compression.

### Maintenance

- Windows CI validates the embedded JavaScript syntax before building.

## [0.11.1] - 2026-09-07

### Added

- Configurable maximum worker-thread limit with `Auto` as the default.
- HDD single-thread safeguard, enabled by default.

### Changed

- SSD analysis and compression can run concurrently using the configured worker limit.
- `Auto` uses up to six workers on storage reported by Windows as having no seek penalty.
- HDD analysis uses a small number of contiguous sample windows to reduce seek overhead.
- HDDs use one worker by default; unknown storage also uses one worker.

### Maintenance

- Release workflow now requires the current `master` commit and blocks publishing while non-draft pull requests targeting `master` are open.

## [0.11.0] - 2026-09-07

### Added

- Configurable minimum estimated savings threshold in Settings; default is 1%.
- Estimated post-compaction size and additional savings during analysis.
- Windows x64 MSVC build and test workflow on stable Rust.

### Changed

- Default compression algorithm is now LZX.
- Analysis and compression now use the same sampled compressibility threshold logic.
- Removed blanket file-extension exclusions; files are judged by sampled contents instead of filename.
- Default exclusions are limited to Windows, System Volume Information, and Windows-managed root `$*` paths.
- Exclusion matching is case-insensitive and ignores blank or whitespace-only entries.
- Compression progress is byte-aware rather than relying only on file count.
- Incompressible-cache keys now include path, file size, and modification time so changed files are automatically reconsidered.
- The cache backing file is now `incompressible-v2.dat` to avoid stale path-only entries.
- Estimated-savings output has a dedicated legend marker in the GUI.
- Removed decorative button and navigation glyphs from the GUI.
- Updated the compression-mode labels to show LZX as the default.

### Fixed

- Correctly treat `DeviceIoControl` as returning a Win32 `BOOL` rather than an HRESULT.
- Correctly surface Win32 errors while handling `ERROR_COMPRESSION_NOT_BENEFICIAL` as a normal no-benefit result.
- Open WOF targets with the read-data/write-attributes access required by the operation.
- Skip encrypted, sparse, offline, reparse-point, and NTFS-compressed special files before WOF classification.
- Use zero-safe progress calculations and saturating size arithmetic to avoid invalid progress or unsigned underflow/overflow edge cases.
- Preserve clear stopped and cancelled state handling.

### Maintenance

- Updated GitHub Actions to current `actions/checkout` and `actions/upload-artifact` majors.
- Removed deprecated Cargo config naming and legacy WinAPI struct-macro warnings.
- Removed unused background helper code.
- Updated project documentation and package metadata for this maintained fork while preserving upstream attribution.
- Reworked user-facing documentation and About text to use concise project wording.

## [0.10.1] - 2020-12-22

### Fixed

- Avoid high CPU usage in GUI loop ([#42])

## [0.10.0] - 2020-12-19

### Changed

- Update dependencies
- Minor UI tweaks due to changes in DPI handling
- Migrate to new dialog crates
- More small internal improvements by @Dr-Emann, thanks! ([#30], [#32])

### Fixed

- Exclusively lock files prior to compaction (should fix [#40], thanks @A-H-M)

## [0.9.0] - 2020-03-03

### Added

- Preserve file timestamps following compression/decompression ([#16])

## [0.8.0] - 2020-02-29

### Added

- Excluded directories now get skipped entirely ([#8])

### Changed

- Paused jobs no longer poll ([#10], @Dr-Emann)
- Less refcounting ([#9], @Dr-Emann)

### Fixed

- Tests ([#11])

## [0.7.1] - 2019-07-17

### Added

- Initial release

[Unreleased]: https://github.com/wefalltomorrow/Compactor/compare/v0.11.5...HEAD
[0.11.5]: https://github.com/wefalltomorrow/Compactor/compare/v0.11.4...v0.11.5
[0.11.4]: https://github.com/wefalltomorrow/Compactor/compare/v0.11.3...v0.11.4
[0.11.3]: https://github.com/wefalltomorrow/Compactor/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/wefalltomorrow/Compactor/releases/tag/v0.11.2
[0.11.1]: https://github.com/wefalltomorrow/Compactor/releases/tag/v0.11.1
[0.11.0]: https://github.com/wefalltomorrow/Compactor/releases/tag/v0.11.0
[0.7.1]: https://github.com/Freaky/Compactor/releases/tag/v0.7.1
[0.8.0]: https://github.com/Freaky/Compactor/releases/tag/v0.8.0
[0.9.0]: https://github.com/Freaky/Compactor/releases/tag/v0.9.0
[0.10.0]: https://github.com/Freaky/Compactor/releases/tag/v0.10.0
[0.10.1]: https://github.com/Freaky/Compactor/releases/tag/v0.10.1
[#6]: https://github.com/Freaky/Compactor/issues/6
[#8]: https://github.com/Freaky/Compactor/issues/8
[#9]: https://github.com/Freaky/Compactor/pull/9
[#10]: https://github.com/Freaky/Compactor/pull/10
[#11]: https://github.com/Freaky/Compactor/pull/11
[#16]: https://github.com/Freaky/Compactor/issues/16
[#30]: https://github.com/Freaky/Compactor/pull/30
[#32]: https://github.com/Freaky/Compactor/pull/32
[#40]: https://github.com/Freaky/Compactor/issues/40
[#42]: https://github.com/Freaky/Compactor/issues/42
