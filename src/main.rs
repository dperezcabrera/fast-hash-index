use anyhow::{anyhow, Context, Result};
use clap::{ArgAction, Parser, ValueEnum};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Algo {
    Blake3,
    Xxh3,
}

#[derive(Debug, Clone)]
struct Entry {
    rel_path: String,
    size: u64,
    tstamp: u64,
    hash_hex: String,
}

#[derive(Parser, Debug)]
#[command(
    about = "Indexes a directory with file hashes and prints diff against a previous state file",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    state_file: PathBuf,
    dir: PathBuf,

    #[arg(short = 'x', long = "exclude")]
    excludes: Vec<String>,

    #[arg(long = "algo", value_enum, default_value_t = Algo::Blake3)]
    algo: Algo,

    #[arg(long = "no-write", action = ArgAction::SetTrue)]
    no_write: bool,

    #[arg(long = "follow-symlinks", action = ArgAction::SetTrue)]
    follow_symlinks: bool,
    
    #[arg(long = "target")]
    target: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let root = fs::canonicalize(&cli.dir)
        .with_context(|| format!("Failed to resolve directory: {:?}", cli.dir))?;

    let target_abs: Option<PathBuf> = if let Some(t) = &cli.target {
        let abs = if t.is_absolute() {
            t.clone()
        } else {
            std::env::current_dir()
                .with_context(|| "Failed to get current working directory")?
                .join(t)
        };
        Some(abs)
    } else {
        None
    };

    if let Some(ref tgt) = target_abs {
        let root_can = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let tgt_can = fs::canonicalize(tgt).unwrap_or_else(|_| tgt.clone());

        if root_can == tgt_can {
            return Err(anyhow!("Target (--target) cannot be the same as source."));
        }
        if tgt_can.starts_with(&root_can) || root_can.starts_with(&tgt_can) {
            return Err(anyhow!("Source and target cannot contain each other."));
        }
    }

    let old_map = read_state_file_map(&cli.state_file).unwrap_or_default();

    let globset = build_globset(&cli.excludes)?;

    let paths = collect_files(&root, &globset, cli.follow_symlinks)?;

    let entries = hash_entries(&root, &paths, cli.algo)?;

    let new_map: HashMap<String, Entry> = entries
        .into_iter()
        .map(|e| (e.rel_path.clone(), e))
        .collect();

    let changes = diff_maps(&old_map, &new_map);

    print_changes(&changes)?;

    if let Some(ref target) = target_abs {
        if !target.exists() {
            fs::create_dir_all(target)
                .with_context(|| format!("Failed to create target directory: {target:?}"))?;
        }

        for ch in &changes {
            match ch {
                Change::Added(rel) | Change::Updated(rel) => {
                    let src = root.join(rel);
                    let dst = target.join(rel);

                    if let Some(parent) = dst.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("Failed to create parent directory in target: {parent:?}")
                        })?;
                    }

                    copy_with_permissions(&src, &dst)
                        .with_context(|| format!("Failed copying '{src:?}' -> '{dst:?}'"))?;
                }
                Change::Deleted(rel) => {
                    let dst = target.join(rel);
                    if dst.exists() {
                        match fs::metadata(&dst) {
                            Ok(md) if md.is_file() => {
                                fs::remove_file(&dst).with_context(|| {
                                    format!("Failed to delete in target: {dst:?}")
                                })?;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    if !cli.no_write {
        write_state_file(&cli.state_file, &new_map)?;
    }

    Ok(())
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();

    let mut expanded: Vec<String> = Vec::new();
    for pat in patterns {
        expanded.push(pat.clone());
        let looks_like_dir = !pat.contains('*') && !pat.ends_with('/') && !pat.ends_with('\\');
        if looks_like_dir {
            expanded.push(format!("{}/**", pat));
            expanded.push(format!("**/{}/**", pat.trim_start_matches("./")));
        }
    }

    for pat in expanded {
        let glob = GlobBuilder::new(&pat)
            .case_insensitive(false)
            .literal_separator(true)
            .build()
            .with_context(|| format!("Invalid exclude pattern: {pat}"))?;
        builder.add(glob);
    }

    Ok(builder.build()?)
}

fn collect_files(root: &Path, globset: &GlobSet, follow_symlinks: bool) -> Result<Vec<PathBuf>> {
    let mut walker = WalkDir::new(root).follow_links(follow_symlinks).into_iter();
    let mut files = Vec::new();

    while let Some(entry_res) = walker.next() {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                eprintln!("Warning: failed to read an entry: {err}");
                continue;
            }
        };

        let ft = entry.file_type();
        let rel = path_to_rel_unix(root, entry.path());

        if ft.is_dir() {
            if globset.is_match(&rel) {
                walker.skip_current_dir();
            }
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        if globset.is_match(&rel) {
            continue;
        }

        files.push(entry.into_path());
    }

    Ok(files)
}

fn path_to_rel_unix(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

fn hash_entries(root: &Path, files: &[PathBuf], algo: Algo) -> Result<Vec<Entry>> {
    let results: Result<Vec<_>> = files
        .par_iter()
        .map(|abs_path| -> Result<Entry> {
            let rel = path_to_rel_unix(root, abs_path);

            let meta = fs::metadata(abs_path)
                .with_context(|| format!("Failed to read metadata for {abs_path:?}"))?;
            let size = meta.len();
            let tstamp = file_timestamp(&meta);

            let hash_hex = match algo {
                Algo::Blake3 => hash_blake3(abs_path)?,
                Algo::Xxh3 => hash_xxh3(abs_path)?,
            };

            Ok(Entry {
                rel_path: rel,
                size,
                tstamp,
                hash_hex,
            })
        })
        .collect();

    let mut entries = results?;
    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(entries)
}

fn file_timestamp(meta: &fs::Metadata) -> u64 {
    let created = meta.created().ok();
    let modified = meta.modified().ok();

    let ts = created.or(modified).unwrap_or(SystemTime::UNIX_EPOCH);
    ts.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hash_blake3(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open for hashing (blake3): {path:?}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_xxh3(path: &Path) -> Result<String> {
    use xxhash_rust::xxh3::Xxh3;
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open for hashing (xxh3): {path:?}"))?;
    let mut state = Xxh3::new();
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        state.update(&buf[..n]);
    }

    let digest128 = state.digest128();
    Ok(format!("{digest128:032x}"))
}

fn read_state_file_map(path: &Path) -> Result<HashMap<String, Entry>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let file = File::open(path).with_context(|| format!("Failed to open previous state: {path:?}"))?;
    let reader = BufReader::new(file);

    let mut map = HashMap::new();
    for (lineno, line_res) in reader.lines().enumerate() {
        let line = match line_res {
            Ok(s) => s,
            Err(err) => {
                eprintln!("Warning: invalid line {} (I/O): {err}", lineno + 1);
                continue;
            }
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        if parts.len() != 4 {
            eprintln!("Warning: invalid format at line {}: {line}", lineno + 1);
            continue;
        }
        let rel = parts[0].to_string();
        let size = parts[1].parse::<u64>().unwrap_or(0);
        let tstamp = parts[2].parse::<u64>().unwrap_or(0);
        let hash_hex = parts[3].to_string();

        map.insert(
            rel.clone(),
            Entry {
                rel_path: rel,
                size,
                tstamp,
                hash_hex,
            },
        );
    }
    Ok(map)
}

fn write_state_file(path: &Path, map: &HashMap<String, Entry>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create state file directory: {parent:?}"))?;
    }
    let file = File::create(path).with_context(|| format!("Failed to create state file: {path:?}"))?;
    let mut w = BufWriter::new(file);

    let mut ordered: BTreeMap<&String, &Entry> = BTreeMap::new();
    for (k, v) in map {
        ordered.insert(k, v);
    }

    for (_k, e) in ordered {
        writeln!(w, "{}:{}:{}:{}", e.rel_path, e.size, e.tstamp, e.hash_hex)?;
    }
    w.flush()?;
    Ok(())
}

#[derive(Debug)]
enum Change {
    Added(String),
    Updated(String),
    Deleted(String),
}

fn diff_maps(old: &HashMap<String, Entry>, new: &HashMap<String, Entry>) -> Vec<Change> {
    let mut changes = Vec::new();

    for (path, e_new) in new {
        match old.get(path) {
            None => changes.push(Change::Added(path.clone())),
            Some(e_old) => {
                if e_old.hash_hex != e_new.hash_hex {
                    changes.push(Change::Updated(path.clone()));
                }
            }
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            changes.push(Change::Deleted(path.clone()));
        }
    }

    changes.sort_by(|a, b| {
        let key_a = match a {
            Change::Added(p) => (0, p),
            Change::Updated(p) => (1, p),
            Change::Deleted(p) => (2, p),
        };
        let key_b = match b {
            Change::Added(p) => (0, p),
            Change::Updated(p) => (1, p),
            Change::Deleted(p) => (2, p),
        };
        key_a.cmp(&key_b)
    });

    changes
}

fn print_changes(changes: &[Change]) -> Result<()> {
    let mut out = io::stdout().lock();
    for c in changes {
        match c {
            Change::Added(p) => writeln!(out, "A: {p}")?,
            Change::Updated(p) => writeln!(out, "U: {p}")?,
            Change::Deleted(p) => writeln!(out, "D: {p}")?,
        }
    }
    Ok(())
}

fn copy_with_permissions(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst).with_context(|| format!("Failed copying '{src:?}' -> '{dst:?}'"))?;

    let src_md = fs::metadata(src)
        .with_context(|| format!("Failed to read source metadata: {src:?}"))?;
    let src_perm = src_md.permissions();

    #[cfg(unix)]
    {
        let mode = PermissionsExt::mode(&src_perm);
        let dst_perm = std::fs::Permissions::from_mode(mode);
        fs::set_permissions(dst, dst_perm)
            .with_context(|| format!("Failed to apply permissions (mode {mode:o}) to: {dst:?}"))?;
    }

    #[cfg(windows)]
    {
        let readonly = src_perm.readonly();
        let mut dst_perm = fs::metadata(dst)
            .with_context(|| format!("Failed to read target metadata: {dst:?}"))?
            .permissions();
        dst_perm.set_readonly(readonly);
        fs::set_permissions(dst, dst_perm)
            .with_context(|| format!("Failed to apply permissions (readonly={readonly}) to: {dst:?}"))?;
    }

    let mtime = filetime::FileTime::from_last_modification_time(&src_md);
    let atime = filetime::FileTime::from_last_access_time(&src_md);

    filetime::set_file_times(dst, atime, mtime)
        .with_context(|| format!("Failed to apply timestamps to: {dst:?}"))?;

    Ok(())
}

