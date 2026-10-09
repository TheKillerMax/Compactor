# Testing and native Windows verification

## Before merging

Use Windows 10/11 (or a supported Windows CI image), a **local NTFS temporary
directory**, stable Rust MSVC x64, Microsoft C++ build tools and Node.js 18 or
newer. The CI workflow runs the following gates:

```powershell
cargo fmt --all -- --check
node --check src/ui/app.js
node --test tests/ui.test.cjs
cargo build --release --locked --target x86_64-pc-windows-msvc
cargo test --workspace --release --locked --target x86_64-pc-windows-msvc
cargo test --release --locked --target x86_64-pc-windows-msvc -- --ignored --test-threads=1
```

The final command is essential. Tests marked `ignore` require actual WOF and
NTFS; they are not equivalent to the ordinary unit tests. The CI and release
workflows explicitly run them and **fail when a native support probe or an
assertion fails**.
They use their own temporary files and do not modify Cargo.lock, a game
installation, or user-selected files.

The native fixtures cover all four codecs, backing detection and scan
classification, unchanged bytes and modification times, before/after allocation versus
`GetCompressedFileSizeW`, decompression, Thorough bypass of a pessimistic
sample, threshold restoration, already-compressed preservation, active writer
conflicts, hardlink protection and canonical-root containment. A separate
Windows unit test exercises cooperative locking from different threads using
independent mutex handles.

## Portable checks

These can be run without Windows, with the stated scope:

```sh
node --test tests/ui.test.cjs
cargo test -p hashfilter --locked
rustc --edition 2018 --test src/policy.rs -o policy-tests
./policy-tests
```

The Node tests execute the actual embedded JavaScript and its messages using a
minimal DOM adapter. They verify behaviour and safe text handling; they do not
run the MSHTML engine. The pure Rust policy tests cover thresholds, invalid
values, cluster rounding and overflow boundaries. Hashfilter tests exercise
real files and controlled write/flush/sync failures.

The Windows Rust sources can also be type-checked from Linux with the
`x86_64-pc-windows-gnu` Rust target and a compatible GCC/MinGW toolchain:

```sh
cargo check --locked --target x86_64-pc-windows-gnu --tests
cargo test --workspace --no-run --locked --target x86_64-pc-windows-gnu
```

The second command also compiles and links every workspace test executable,
without running it. Add `--offline` when the dependencies are already cached.
Resource compilation remains enabled. The legacy embedded C dependencies need
a compatible compiler. Neither command executes WOF or native MSHTML, and
neither replaces the Windows MSVC build and integration gate. Native Windows
execution must be verified separately; cross-compilation does not establish
that those tests passed.

## Manual checks on Windows

1. Copy a representative, inactive folder into an isolated test location. Record
   its contents and allocation. Compare LZX + Sampled with LZX + Thorough on
   separate identical copies: total allocated bytes, elapsed time, CPU impact,
   report counts and application loading behaviour. Do not extrapolate a
   saving from the sample or from another user's dataset.
2. Set a high minimum percentage and confirm low-benefit files are restored.
   Review failures if free space is tight; do not assume restoration succeeded
   just because it was requested.
3. Analyse, change exclusions or the analysis mode, then confirm compression
   requires a new analysis. Check a literal folder such as `Title [2024]`.
4. Pause, resume and stop both transformations. Close the window while paused
   and while a file is active. Active native calls can finish; no new files
   should be scheduled and the process should then exit.
5. Include a known DirectStorage runtime under an excluded `bin` directory.
   Confirm the game root is protected. Cancel the preflight and confirm no file
   is transformed.
6. Decompress a folder containing a file that cannot be expanded. Confirm a
   single attempt, a visible error and retained WOF classification. The
   `Decompress + exclude` action must not claim full restoration or add the
   exclusion while WOF files remain.
7. Check the settings, file/folder viewer, empty summary and operation report at
   common Windows display scales. Test cancelling the folder picker and
   attempting invalid numeric settings.

Keep the native CI result and any workload measurements with the PR. A source
diff, formatting pass, cross-check or mocked UI test alone is not evidence that
native WOF operations passed.
