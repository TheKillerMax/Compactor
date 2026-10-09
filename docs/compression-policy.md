# Compression policy

## Choose the amount of work, then the codec

Compactor keeps files directly usable through Windows Overlay Filter (WOF).
The supported FILE provider formats remain XPRESS4K, XPRESS8K, XPRESS16K and
LZX. Microsoft describes LZX as the most compact choice in `compact /EXE`.
This is a reason to offer LZX for maximum space savings, not a guarantee that
every file or workload benefits equally.

| Setting | Candidate selection | Tradeoff |
| --- | --- | --- |
| Sampled (default) | A generic 8 KiB sampler chooses potentially worthwhile files | Less work, but can miss savings that the selected codec could achieve |
| Thorough | All otherwise eligible files are attempted unless the same policy already measured them as not worthwhile | Maximum coverage with the selected codec, at the cost of more CPU time and writes |

LZX + Thorough is the option for users who want to spend more work looking for
additional savings. The choice does not introduce a new compression format,
change file names, or convert working files into archives. Both modes keep the
existing HDD and LZX worker limits, DirectStorage safeguard and worker priority.

No data-dependent benchmark has been run on the user's files. No fixed savings
percentage or improvement over the previous version is claimed. The generic
sample is not a native XPRESS/LZX benchmark. In particular, an arbitrary 10%
LZX projection improvement has been removed.

## What qualifies as worthwhile

For each new compression operation, the worker records the allocation before
and after compression under a protected file identity. Compression is retained
only when:

1. The selected WOF algorithm is actually present after the operation.
2. The logical file length is unchanged.
3. The allocation reduction meets the configured percentage of the **initial
   allocation**, rounded up to a whole byte.
4. At least one allocation unit (the target NTFS cluster size) is saved.

The default percentage is 1%. Setting it to zero still requires a positive
saving of one cluster; it never means retaining a file that grew.
`ERROR_COMPRESSION_NOT_BENEFICIAL` is a normal negative result.

If a newly compressed file does not meet the threshold, Compactor requests
restoration to uncompressed form and verifies the backing state afterwards.
Restoration can need temporary free space for the entire logical stream. The
space check is a snapshot, not a reservation; another process can still consume
space. If a measurement, operation or restoration fails, the error report says
so and a rescan is required. A failure is never silently recorded as success.

The allocation query uses `FileCompressionInfo`, not the allocation of the
underlying sparse stream. For WOF it opens a normal view while retaining the
protected handle and compares volume/file identity before using it. It does
not enumerate or add the hidden WOF data stream. A nonempty WOF file with an
ambiguous zero allocation is rejected as unmeasurable. Native Windows tests
must pass the comparison with `GetCompressedFileSizeW` before this change is
merged or released.

## Files and operations that are protected

- Compress preserves existing WOF compression. It does not automatically expand
  XPRESS files or try multiple algorithms in sequence. There is no dependency
  on an undocumented in-place algorithm migration contract.
- Users can explicitly Decompress, Analyse and then Compress with another
  method. Expansion requires free space and the usual file access rights.
- Whole-drive roots, network targets, non-NTFS targets, protected Windows paths
  and folder aliases through reparse points are refused.
- Every worker checks the final opened path against the canonical root captured
  for the scan. Replacing a nested directory with a junction cannot redirect a
  queued operation outside that root.
- Unsupported providers and versions are left unchanged. Malformed API
  responses and failed inspections are reported as errors.
- Other reparse points, sparse files without supported WOF backing, encrypted,
  offline, legacy NTFS-compressed, system, temporary and read-only files are
  skipped. Hardlinks are not transformed because another name can lie outside
  the selected tree and accounting would otherwise count the same storage twice.
- Active content writers/deleters conflict with the file sharing mode. A
  per-identity mutex coordinates cooperating Compactor instances. Unrelated
  tools that change WOF attributes do not participate in that mutex.
- DirectStorage protection searches independently of compression exclusions.
  An excluded `bin` folder therefore cannot hide a known runtime signal from
  the protection pass. The heuristic cannot detect every application or layout;
  disabling it is an explicit user choice.

These checks reduce known problems; they do not establish that arbitrary
applications perform equally with every codec. Write-heavy databases, VM
images, active logs and installations being updated are poor candidates for
this kind of filesystem compression.

## Cache and operation reports

The v3 cache contains only measured negative results. Its key includes path,
logical length, modification time, codec, analysis mode, threshold and cluster
size. A Sampled rejection is not proof of incompressibility and is not cached.
The fingerprint is metadata-based, not a cryptographic content hash.
Changing the relevant policy or metadata reconsiders the file; v2 entries do
not suppress new attempts.

Each run reports changed files, below-threshold results, skips and failures.
Error details are limited to the first 20 entries while the count includes all
errors. Reported operation savings use the before/after measurements of
completed files. They do not compare a stale analysis snapshot with the final
folder or conceal failed files inside a success count.

## Primary references

- [Microsoft: compact](https://learn.microsoft.com/windows-server/administration/windows-commands/compact)
- [Microsoft: WOF_FILE_COMPRESSION_INFO_V1](https://learn.microsoft.com/windows/win32/api/wofapi/ns-wofapi-wof_file_compression_info_v1)
- [Microsoft: WofSetFileDataLocation](https://learn.microsoft.com/windows/win32/api/wofapi/nf-wofapi-wofsetfiledatalocation)
- [Microsoft: WofIsExternalFile and buffer sizing](https://learn.microsoft.com/windows/win32/api/wofapi/nf-wofapi-wofisexternalfile)
- [Microsoft: FSCTL_GET_EXTERNAL_BACKING](https://learn.microsoft.com/windows-hardware/drivers/ifs/fsctl-get-external-backing)
- [Microsoft: FSCTL_DELETE_EXTERNAL_BACKING](https://learn.microsoft.com/windows-hardware/drivers/ifs/fsctl-delete-external-backing)
- [Microsoft: FILE_COMPRESSION_INFO](https://learn.microsoft.com/windows/win32/api/winbase/ns-winbase-file_compression_info)
- [Microsoft: GetCompressedFileSizeW](https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-getcompressedfilesizew)

References checked on 2026-10-09. See [TESTING.md](TESTING.md) for the required
native verification and manual workload checks.
