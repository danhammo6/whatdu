# whatdu

**What’s using your disk, and what to do about it.**

A Rust, ncdu-style terminal browser with multi-threaded scanning and deletion,
live progress, and a clear view of where your disk space goes.

Inspect disk usage, navigate directories, rescan with `r`, and clean up with `d`.
Deletion is permanent and requires confirmation by default.

## Build and run

Requires a current stable Rust toolchain.

```sh
cargo build --release
./target/release/whatdu /path/to/scan

# Install on your PATH
cargo install --path .
```

```sh
whatdu .                     # Interactive browser
whatdu -t 8 /var             # Eight scan workers
whatdu -a .                  # Apparent size instead of allocated space
whatdu -x /                  # Stay on the root filesystem (Unix)
whatdu --no-ui .             # Immediate children, largest first
whatdu --json . > usage.json # Complete tree with both size measures
whatdu --count-links .       # Count each hard link separately
```

Non-terminal input or output automatically selects plain-text mode. Scanning
progress goes to stderr when it is a terminal; JSON on stdout stays clean.
Initial scans and `r` rescans show a running allocated-space total alongside
the entry count. This uses allocated blocks, not logical lengths, even in
apparent-size mode. Hard links within the scanned subtree count once unless
`--count-links` is set. A subtree rescan's running total covers that subtree;
final tree accounting also deduplicates links against cached entries elsewhere.
Plain-text output is also useful with screen readers.

## Browser keys

- `Up` / `Down` or `k` / `j`: select an entry.
- `Enter` / `Right` / `l`: open a directory.
- `Left` / `Backspace` / `h`: return to the parent.
- `Home` / `End`: jump to the first / last entry.
- `PageUp` / `PageDown`: move by a page.
- `a`: toggle the primary size column and size sorting between disk usage / logical size.
- `s`: toggle size / name sorting.
- `/`: type a case-insensitive name filter, then `Enter` to apply.
- `Esc`: clear the filter or close help.
- `?`: toggle help.
- `r`: rescan the directory currently being viewed, including its descendants.
- `d`: permanently delete the selected file or directory recursively.
- `q` or `Ctrl-C`: exit.

Directories have a `/` suffix, symlinks an `@` suffix. Scan failures are marked
`[errors]`; the selected entry's error appears in the footer when available.
The root path is the upper navigation boundary. Empty directories and individual
file roots are supported.

Both **Disk usage** (allocated space) and **Logical** (apparent size) are shown
in the browser and plain-text output. Percentages use the primary size column.
Logical size is not disk usage and can exceed your disk's capacity. For example,
Docker's `Docker.raw` may have a 1 TiB logical length but occupy only 66 GiB:
unwritten sparse regions consume no allocated blocks. Cloud placeholders can
likewise report their remote logical lengths with little or no local allocation.
Use the default disk-usage mode to investigate local space consumption; `a`
or `--apparent-size` shows logical lengths, not additional disk consumption.

## Deletion

`d` opens a confirmation dialog with **Yes**, **No** (the default), and
**Don't ask me again**. Use arrows or Tab to select and Enter to confirm;
Esc, `n`, or `q` cancels. **Don't ask me again** deletes the current selection
and disables confirmations for this browser session only. The footer warns when
confirmations are disabled. Restart whatdu to restore prompts.

Deletion is permanent, not a move to Trash. Symlinks are unlinked without
following their targets. The scan root cannot be deleted. Unix inode checks
reject replaced entries, parent symlink checks reject redirected paths, and
deletion refuses trees crossing filesystem boundaries. These checks are not
a security boundary against concurrent, malicious filesystem changes; avoid
deleting in trees being modified by other processes. Non-Unix platforms do not
have the Unix inode and device checks.

Successful deletion removes the entry from the in-memory snapshot and refreshes
only ancestor directory metadata. Totals and hard-link ownership are recomputed
from cached metadata, without rescanning the filesystem. After a failed attempt,
only the selected subtree is rescanned to account for possible partial deletion.
Unrelated filesystem changes are not refreshed. Failures appear in the footer;
a recursive failure can leave some contents already deleted.

Deletion runs in the background on a bounded Rayon pool, using the same `-t`
worker count as scanning. Sibling entries are removed in parallel; a directory
is removed only after all of its children succeed. A preflight traversal checks
filesystem boundaries before any removal and captures identities for rechecking.
This plan uses memory proportional to the selected subtree's entry count.

The progress screen updates approximately every 100 ms, showing the phase
(checking, deleting, updating totals), elapsed time, worker count, and counts of
checked and successfully deleted entries. Files, symlinks, and directories each
count as one entry. No other delete can start until this operation finishes.
Deletion also shows **allocated processed**: the sum of the allocated sizes of
successfully removed entries, sampled before removal. This is **not space freed**.
Hard-linked names each contribute their reported allocation, even when other
links retain the data; snapshots and copy-on-write sharing can also retain blocks.
The byte counter is a progress measure, not a claim about reclaimed disk capacity.
Esc, `q`, or Ctrl-C requests cancellation and returns to the browser after active
operations finish and totals refresh. Cancellation cannot restore deleted entries
or interrupt an in-flight filesystem call. Progress stays visible during refresh.
Parallel deletion may not be faster on every filesystem, especially rotational
disks or network shares; use `-t 1` for serial traversal and deletion.
Deleting a cloud-provider entry can also delete its remote copy according to
the provider's behavior.

## Scanning and accounting

Directory enumeration and metadata reads run on a bounded Rayon work-stealing
pool, defaulting to the OS-reported available CPU parallelism. Nested directories
and files are scheduled in parallel without creating a thread per directory.
Directory handles close before descent. After traversal, a sequential accounting
pass computes subtree totals and deterministically deduplicates hard links.

Allocated size uses Unix `st_blocks * 512`; apparent size uses metadata length.
Both include directory and symlink metadata. Symlinks are never followed,
including a symlink supplied as the root. Multiply-linked non-directory inodes
are counted only at the first lexically sorted path by default. Every directory
entry still contributes to the item count. Filesystem boundary detection and
hard-link deduplication are Unix features; other platforms use logical lengths
for allocated size. Copy-on-write sharing and compression are not independently
accounted for, so this is not a physical storage ownership report.

Unreadable or disappearing entries do not abort the scan; they are marked and
their unknown contents omitted. A root metadata failure aborts. Exit codes:
`0` success, `1` fatal failure, `2` incomplete scan (clap also uses `2` for invalid
arguments). The tree is an in-memory snapshot, not a live view or atomic snapshot:
concurrent filesystem changes can affect totals. Memory use scales with entry
count, and extremely deep trees are limited by recursive traversal and accounting.
Increasing worker count may slow down rotational disks or network filesystems.

JSON paths are lossily UTF-8 encoded for portability; non-UTF-8 names can therefore
collide in exports. Terminal control characters in displayed names are escaped.
Rescanning runs in the background with an entry-count progress screen; wait for
completion before issuing another command. It retains the current directory,
filter, and sorting, and updates ancestor totals and hard-link accounting.
Rescanning a nested directory does not read unrelated subtrees. At the scan root,
`r` refreshes the entire tree. A failed rescan leaves the existing snapshot intact
and reports the error in the footer.

There is no exclusion pattern or ncdu export-format compatibility
in this initial implementation.

## Tests

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
python3 tests/tui_delete.py # Unix pseudo-terminal test; disposable fixtures only
```
