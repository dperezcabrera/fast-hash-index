
# Fast Hash Index

A command-line tool to **index a directory with file hashes**, detect changes (added, updated, deleted files), and optionally **synchronize them** to a target directory.  
It supports two hash algorithms (`blake3` and `xxh3`), glob-based exclusions, and preserves **permissions and timestamps** when synchronizing.

---

## Features

- Indexes all regular files in a directory.
- Stores file metadata in a *state file* (`path:size:timestamp:hash`).
- Detects changes compared to the previous state:
  - **A:** Added  
  - **U:** Updated (hash changed)  
  - **D:** Deleted
- Supports exclusion patterns (`--exclude '**/target/**'`).
- Choice of hash algorithm:
  - `blake3` (default, cryptographic, fast).
  - `xxh3` (very fast, non-cryptographic).
- Can follow symbolic links (`--follow-symlinks`).
- Optional **synchronization** with a target directory (`--target`), preserving file contents, permissions, and timestamps.

---

## Build

```bash
cargo build --release
````

The binary will be available at:

```bash
target/release/fast-hash-index
```

---

## Usage

```bash
fast-hash-index [OPTIONS] <STATE_FILE> <DIR>
```

* `<STATE_FILE>` – path to the state file to read/write.
* `<DIR>` – root directory to index.

### Options

* `-x, --exclude <PATTERN>`
  Exclude files/directories matching a glob pattern. Can be repeated.
  Example:

  ```bash
  --exclude '**/target/**' --exclude '*.log'
  ```

* `--algo <blake3|xxh3>`
  Select hash algorithm (default: `blake3`).

* `--no-write`
  Do not write the updated state file (only print changes).

* `--follow-symlinks`
  Follow symbolic links during scanning.

* `--target <DIR>`
  Synchronize detected changes into `<DIR>`:

  * Added/Updated files are copied.
  * Deleted files are removed.
  * Permissions and timestamps are preserved.

---

## Examples

### 1. Index a directory

```bash
fast-hash-index state.txt ./my-project
```

### 2. Exclude patterns

```bash
fast-hash-index state.txt ./my-project \
  --exclude '**/target/**' \
  --exclude '*.tmp'
```

### 3. Use xxh3 (faster, non-cryptographic)

```bash
fast-hash-index state.txt ./my-project --algo xxh3
```

### 4. Dry run (show changes but don’t update state file)

```bash
fast-hash-index state.txt ./my-project --no-write
```

### 5. Synchronize to another directory

```bash
fast-hash-index state.txt ./src --target ./backup
```

* Copies new/updated files to `./backup`.
* Deletes files in `./backup` that were deleted in `./src`.
* Preserves file permissions and timestamps.

---

## Output format

Each change is printed to stdout:

```
A: path/to/new_file.txt
U: path/to/changed_file.rs
D: path/to/removed_file.log
```

---

## Notes

* The state file is overwritten after each run (unless `--no-write` is used).
* Target directory must not overlap with the source directory.
* On Unix, file **mode bits** (permissions) are preserved.
* On all platforms, **timestamps** (mtime/atime) are preserved using the `filetime` crate.


