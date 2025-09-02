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

/// Algoritmo de hash: blake3 (por defecto) o xxh3 (muy rápido, no criptográfico).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Algo {
    Blake3,
    Xxh3,
}

#[derive(Debug, Clone)]
struct Entry {
    rel_path: String,
    size: u64,
    tstamp: u64,    // created() si existe; si no, modified(); en segundos epoch
    hash_hex: String,
}

#[derive(Parser, Debug)]
#[command(
    about = "Indexa un directorio con hashes y compara contra un fichero de estado previo",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    /// Fichero de estado (se leerá si existe y se sobrescribirá con el nuevo índice)
    state_file: PathBuf,
    /// Directorio a indexar (raíz)
    dir: PathBuf,

    /// Exclusiones (glob). Puede repetirse. Ej: --exclude '**/target/**' --exclude '*.log'
    #[arg(short = 'x', long = "exclude")]
    excludes: Vec<String>,

    /// Algoritmo de hash: blake3 (por defecto) o xxh3
    #[arg(long = "algo", value_enum, default_value_t = Algo::Blake3)]
    algo: Algo,

    /// No escribir el fichero de estado tras calcular el índice (solo imprime diff)
    #[arg(long = "no-write", action = ArgAction::SetTrue)]
    no_write: bool,

    /// Seguir enlaces simbólicos (por defecto: no)
    #[arg(long = "follow-symlinks", action = ArgAction::SetTrue)]
    follow_symlinks: bool,
    
    /// Directorio destino para sincronizar los cambios detectados (opcional)
    #[arg(long = "target")]
    target: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Normaliza raíz (origen)
    let root = fs::canonicalize(&cli.dir)
        .with_context(|| format!("No se pudo resolver el directorio: {:?}", cli.dir))?;

    // Resuelve y normaliza destino (si se solicitó)
    let target_abs: Option<PathBuf> = if let Some(t) = &cli.target {
        // Si no existe, lo resolvemos respecto al cwd para tener ruta absoluta estable
        let abs = if t.is_absolute() {
            t.clone()
        } else {
            std::env::current_dir()
                .with_context(|| "No se pudo obtener el directorio actual")?
                .join(t)
        };
        Some(abs)
    } else {
        None
    };

    // Comprobaciones de solapamiento origen/destino
    if let Some(ref tgt) = target_abs {
        // canonicalize si existe; si no, intenta normalizar componentes "limpiando" path
        let root_can = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let tgt_can = fs::canonicalize(tgt).unwrap_or_else(|_| tgt.clone());

        if root_can == tgt_can {
            return Err(anyhow!(
                "El destino (--target) no puede ser el mismo que el origen."
            ));
        }
        if tgt_can.starts_with(&root_can) || root_can.starts_with(&tgt_can) {
            return Err(anyhow!(
                "El origen y el destino no pueden estar contenidos uno dentro del otro."
            ));
        }
    }

    // Lee estado previo (si existe y no está vacío)
    let old_map = read_state_file_map(&cli.state_file).unwrap_or_default();

    // Prepara patrón de exclusiones
    let globset = build_globset(&cli.excludes)?;

    // Reúne ficheros
    let paths = collect_files(&root, &globset, cli.follow_symlinks)?;

    // Calcula índice nuevo (en paralelo)
    let entries = hash_entries(&root, &paths, cli.algo)?;

    // Convierte a mapas para diffs
    let new_map: HashMap<String, Entry> = entries
        .into_iter()
        .map(|e| (e.rel_path.clone(), e))
        .collect();

    // Calcula diffs
    let changes = diff_maps(&old_map, &new_map);

    // Imprime diffs
    print_changes(&changes)?;

    // Si se pasó --target, sincroniza A/U/D hacia el destino
    if let Some(ref target) = target_abs {
        // Crea el directorio raíz del destino si no existe
        if !target.exists() {
            fs::create_dir_all(target)
                .with_context(|| format!("No se pudo crear el directorio destino: {target:?}"))?;
        }

        for ch in &changes {
            match ch {
                Change::Added(rel) | Change::Updated(rel) => {
                    let src = root.join(rel);
                    let dst = target.join(rel);

                    // Asegura el directorio padre en destino
                    if let Some(parent) = dst.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("No se pudo crear el directorio padre en destino: {parent:?}")
                        })?;
                    }

                    // Copia (sobrescribe si existe). No preserva mtime/ pero si permisos.
                    copy_with_permissions(&src, &dst).with_context(|| {
                        format!("Fallo copiando '{src:?}' -> '{dst:?}'")
                    })?;
                }
                Change::Deleted(rel) => {
                    let dst = target.join(rel);
                    if dst.exists() {
                        // Borra solo ficheros; si no es fichero, ignora de forma segura
                        match fs::metadata(&dst) {
                            Ok(md) if md.is_file() => {
                                fs::remove_file(&dst).with_context(|| {
                                    format!("No se pudo borrar en destino: {dst:?}")
                                })?;
                            }
                            _ => {
                                // No es fichero (o no existe); no hacemos nada.
                            }
                        }
                    }
                }
            }
        }
    }

    // Escribe el nuevo índice, salvo que sea --no-write
    if !cli.no_write {
        write_state_file(&cli.state_file, &new_map)?;
    }

    Ok(())
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        // Respetar separador '/' (para rutas relativas tipo Unix, independiente de SO)
        let glob = GlobBuilder::new(pat)
            .case_insensitive(false)
            .literal_separator(true)
            .build()
            .with_context(|| format!("Patrón de exclusión inválido: {pat}"))?;
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
                eprintln!("Aviso: no se pudo leer una entrada: {err}");
                continue;
            }
        };

        let ft = entry.file_type();
        if ft.is_dir() {
            // Si el directorio está excluido, saltar su contenido
            let rel = path_to_rel_unix(root, entry.path());
            if globset.is_match(&rel) {
                let _ = entry.depth().checked_add(1);
            }
            continue;
        }
        if !ft.is_file() {
            // Ignora symlinks a ficheros si follow_symlinks = false (WalkDir ya respeta esa config)
            continue;
        }

        let rel = path_to_rel_unix(root, entry.path());
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
                .with_context(|| format!("No se pudo leer metadata de {abs_path:?}"))?;
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

    // Ordena por ruta para estabilidad (aunque no es estrictamente necesario aquí)
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
        .with_context(|| format!("No se pudo abrir para hash (blake3): {path:?}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024]; // 1 MiB

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
        .with_context(|| format!("No se pudo abrir para hash (xxh3): {path:?}"))?;
    let mut state = Xxh3::new();
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        state.update(&buf[..n]);
    }

    // Usamos XXH3_128 para tener hex más robusto (32 hex chars)
    let digest128 = state.digest128();
    Ok(format!("{digest128:032x}"))
}

fn read_state_file_map(path: &Path) -> Result<HashMap<String, Entry>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let file = File::open(path).with_context(|| format!("No se pudo abrir el estado previo: {path:?}"))?;
    let reader = BufReader::new(file);

    let mut map = HashMap::new();
    for (lineno, line_res) in reader.lines().enumerate() {
        let line = match line_res {
            Ok(s) => s,
            Err(err) => {
                eprintln!("Aviso: línea {} inválida (I/O): {err}", lineno + 1);
                continue;
            }
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        if parts.len() != 4 {
            eprintln!("Aviso: línea {} con formato inválido: {line}", lineno + 1);
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

/// Escribe el índice nuevo (ordenado alfabéticamente) en el fichero de estado.
fn write_state_file(path: &Path, map: &HashMap<String, Entry>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("No se pudo crear el directorio del estado: {parent:?}"))?;
    }
    let file = File::create(path).with_context(|| format!("No se pudo crear el estado: {path:?}"))?;
    let mut w = BufWriter::new(file);

    // Orden determinista
    let mut ordered: BTreeMap<&String, &Entry> = BTreeMap::new();
    for (k, v) in map {
        ordered.insert(k, v);
    }

    for (_k, e) in ordered {
        // path:size:timestamp:hash
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

    // Añadidos y modificados
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
    // Eliminados
    for path in old.keys() {
        if !new.contains_key(path) {
            changes.push(Change::Deleted(path.clone()));
        }
    }

    // Ordena por tipo y ruta para estabilidad
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
    // Copia el contenido (crea/sobrescribe)
    fs::copy(src, dst).with_context(|| format!("Fallo copiando '{src:?}' -> '{dst:?}'"))?;

    // Permisos
    let src_md = fs::metadata(src)
        .with_context(|| format!("No se pudo leer metadata del origen: {src:?}"))?;
    let src_perm = src_md.permissions();

    #[cfg(unix)]
    {
        let mode = PermissionsExt::mode(&src_perm);
        let dst_perm = std::fs::Permissions::from_mode(mode);
        fs::set_permissions(dst, dst_perm)
            .with_context(|| format!("No se pudo aplicar permisos (mode {mode:o}) a: {dst:?}"))?;
    }

    #[cfg(windows)]
    {
        let readonly = src_perm.readonly();
        let mut dst_perm = fs::metadata(dst)
            .with_context(|| format!("No se pudo leer metadata de destino: {dst:?}"))?
            .permissions();
        dst_perm.set_readonly(readonly);
        fs::set_permissions(dst, dst_perm)
            .with_context(|| format!("No se pudo aplicar permisos (readonly={readonly}) a: {dst:?}"))?;
    }

    // Timestamps (mtime y atime)
    let mtime = filetime::FileTime::from_last_modification_time(&src_md);
    let atime = filetime::FileTime::from_last_access_time(&src_md);

    filetime::set_file_times(dst, atime, mtime)
        .with_context(|| format!("No se pudieron aplicar timestamps a: {dst:?}"))?;

    Ok(())
}



