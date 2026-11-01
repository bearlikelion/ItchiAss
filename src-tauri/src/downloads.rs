use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;

use crate::api::{self, Upload};
use crate::state::{self, AppState, ManifestEntry, Settings, err};

const ATTEMPTS: u32 = 3;

#[derive(Deserialize, Clone)]
pub struct QueueItem {
    pub game_id: u64,
    pub key_id: u64,
    pub creator: String,
    pub title: String,
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    game_id: u64,
    upload_id: u64,
    filename: &'a str,
    received: u64,
    total: u64,
    state: &'a str,
    error: Option<String>,
}

enum Stop {
    Cancelled,
    Failed(String),
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Failed(e)
    }
}

fn emit(app: &AppHandle, game_id: u64, u: Option<&Upload>, received: u64, total: u64, state: &str, error: Option<String>) {
    let p = Progress {
        game_id,
        upload_id: u.map_or(0, |u| u.id),
        filename: u.map_or("", |u| u.filename.as_str()),
        received,
        total,
        state,
        error,
    };
    let _ = app.emit("download-progress", p);
}

// Valid on every OS: no Windows-illegal characters, reserved device names, or trailing dots/spaces.
pub fn sanitize(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let out = out.trim().trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        return "_".into();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved { format!("_{out}") } else { out }
}

// Verbatim paths lift the 260 character MAX_PATH limit on Windows.
pub fn long_path(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if p.is_absolute() && !s.starts_with(r"\\?\") {
            return match s.strip_prefix(r"\\") {
                Some(unc) => PathBuf::from(format!(r"\\?\UNC\{unc}")),
                None => PathBuf::from(format!(r"\\?\{s}")),
            };
        }
    }
    p.to_path_buf()
}

// Archive entry name to a path under dest, or None when it would escape dest.
fn safe_join(dest: &Path, name: &str) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    let normalized = name.replace('\\', "/");
    for c in Path::new(&normalized).components() {
        match c {
            Component::Normal(part) => out.push(sanitize(&part.to_string_lossy())),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (out != dest).then_some(out)
}

fn write_entry(path: &Path, reader: &mut dyn Read) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    let mut f = File::create(path).map_err(err)?;
    std::io::copy(reader, &mut f).map_err(err)?;
    f.flush().map_err(err)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Zip,
    TarGz,
    SevenZ,
    Rar,
}

fn archive_kind(name: &str) -> Option<(Kind, &str)> {
    let lower = name.to_ascii_lowercase();
    [(".tar.gz", Kind::TarGz), (".tgz", Kind::TarGz), (".zip", Kind::Zip), (".7z", Kind::SevenZ), (".rar", Kind::Rar)]
        .into_iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|(ext, k)| (k, &name[..name.len() - ext.len()]))
}

fn extract(kind: Kind, archive: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(err)?;
    match kind {
        Kind::Zip => {
            let mut zip = zip::ZipArchive::new(File::open(archive).map_err(err)?).map_err(err)?;
            for i in 0..zip.len() {
                let mut entry = zip.by_index(i).map_err(err)?;
                let Some(path) = safe_join(dest, entry.name()) else { continue };
                if entry.is_dir() {
                    std::fs::create_dir_all(&path).map_err(err)?;
                } else if entry.is_file() {
                    write_entry(&path, &mut entry)?;
                }
            }
            Ok(())
        }
        Kind::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive).map_err(err)?));
            for entry in tar.entries().map_err(err)? {
                let mut entry = entry.map_err(err)?;
                let name = entry.path().map_err(err)?.to_string_lossy().into_owned();
                let Some(path) = safe_join(dest, &name) else { continue };
                match entry.header().entry_type() {
                    tar::EntryType::Directory => std::fs::create_dir_all(&path).map_err(err)?,
                    tar::EntryType::Regular => write_entry(&path, &mut entry)?,
                    _ => {}
                }
            }
            Ok(())
        }
        Kind::SevenZ => sevenz_rust2::decompress_file_with_extract_fn(archive, dest, |entry, reader, _| {
            if let Some(path) = safe_join(dest, entry.name()) {
                if entry.is_directory() {
                    std::fs::create_dir_all(&path)?;
                } else {
                    write_entry(&path, reader).map_err(std::io::Error::other)?;
                }
            }
            Ok(true)
        })
        .map_err(err),
        Kind::Rar => {
            let mut rar = unrar::Archive::new(archive).open_for_processing().map_err(err)?;
            while let Some(header) = rar.read_header().map_err(err)? {
                let entry = header.entry();
                let target = safe_join(dest, &entry.filename.to_string_lossy());
                rar = match target {
                    Some(path) if entry.is_file() => {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).map_err(err)?;
                        }
                        header.extract_to(&path).map_err(err)?
                    }
                    Some(path) => {
                        std::fs::create_dir_all(&path).map_err(err)?;
                        header.skip().map_err(err)?
                    }
                    None => header.skip().map_err(err)?,
                };
            }
            Ok(())
        }
    }
}

pub fn enqueue(app: AppHandle, items: Vec<QueueItem>) {
    let st = app.state::<AppState>();
    let settings = st.settings();
    let Some(token) = settings.token.clone() else { return };
    let sem = st.semaphore(settings.parallel);

    for item in items {
        let (app, settings, token, sem) = (app.clone(), settings.clone(), token.clone(), sem.clone());
        tauri::async_runtime::spawn(async move {
            let st = app.state::<AppState>();
            let uploads = match api::uploads(&st.client, &token, item.game_id, item.key_id).await {
                Ok(u) => u,
                Err(e) => return emit(&app, item.game_id, None, 0, 0, "error", Some(e)),
            };
            let manifest = state::load_manifest(&settings.download_dir);
            let todo: Vec<Upload> = uploads
                .into_iter()
                .filter(|u| u.downloadable() && !settings.skip_uploads.contains(&u.id) && !state::is_current(u, &manifest))
                .collect();
            if todo.is_empty() {
                return emit(&app, item.game_id, None, 0, 0, "done", None);
            }
            for u in todo {
                let cancel = st.cancel_flag(u.id);
                emit(&app, item.game_id, Some(&u), 0, u.size.unwrap_or(0), "queued", None);
                let (app, settings, token, sem, item) = (app.clone(), settings.clone(), token.clone(), sem.clone(), item.clone());
                tauri::async_runtime::spawn(async move {
                    let _permit = sem.acquire_owned().await;
                    run_with_retry(&app, &settings, &token, &item, &u, &cancel).await;
                    app.state::<AppState>().clear_cancel(u.id);
                });
            }
        });
    }
}

async fn run_with_retry(app: &AppHandle, settings: &Settings, token: &str, item: &QueueItem, u: &Upload, cancel: &AtomicBool) {
    for attempt in 1..=ATTEMPTS {
        match download_one(app, settings, token, item, u, cancel).await {
            Ok(()) => return,
            Err(Stop::Cancelled) => return emit(app, item.game_id, Some(u), 0, 0, "cancelled", None),
            Err(Stop::Failed(e)) if attempt == ATTEMPTS => return emit(app, item.game_id, Some(u), 0, 0, "error", Some(e)),
            Err(Stop::Failed(e)) => {
                emit(app, item.game_id, Some(u), 0, 0, "queued", Some(format!("retrying: {e}")));
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
        }
    }
}

fn game_dir(settings: &Settings, item: &QueueItem) -> PathBuf {
    let base = if settings.by_creator { settings.download_dir.join(sanitize(&item.creator)) } else { settings.download_dir.clone() };
    base.join(sanitize(&item.title))
}

fn version_tag(u: &Upload) -> String {
    match (&u.md5_hash, &u.updated_at) {
        (Some(h), _) if h.len() >= 8 => h[..8].to_string(),
        (_, Some(t)) => t.chars().filter(char::is_ascii_alphanumeric).collect(),
        _ => u.id.to_string(),
    }
}

async fn download_one(app: &AppHandle, settings: &Settings, token: &str, item: &QueueItem, u: &Upload, cancel: &AtomicBool) -> Result<(), Stop> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Stop::Cancelled);
    }
    let st = app.state::<AppState>();
    let root = &settings.download_dir;
    let dir = game_dir(settings, item);
    let fs_dir = long_path(&dir);
    tokio::fs::create_dir_all(&fs_dir).await.map_err(err)?;
    let filename = sanitize(&u.filename);
    let file = fs_dir.join(&filename);
    // The version tag keeps a stale partial from being resumed into a newer upload.
    let part = fs_dir.join(format!("{filename}.{}.part", version_tag(u)));

    let mut hasher = Md5::new();
    let mut have = 0u64;
    if let Ok(mut f) = File::open(&part) {
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf).map_err(err)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            have += n as u64;
        }
    }

    let resp = api::download(&st.client, token, u.id, item.key_id, have).await?;
    let resumed = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !resumed {
        hasher = Md5::new();
        have = 0;
    }
    let total = resp.content_length().map(|n| n + have).or(u.size).unwrap_or(0);
    let mut out = tokio::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(&part).await.map_err(err)?;
    let mut received = have;
    let mut last = Instant::now();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            out.flush().await.map_err(err)?;
            return Err(Stop::Cancelled);
        }
        let chunk = chunk.map_err(err)?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(err)?;
        received += chunk.len() as u64;
        if last.elapsed() > Duration::from_millis(250) {
            last = Instant::now();
            emit(app, item.game_id, Some(u), received, total, "downloading", None);
        }
    }
    out.flush().await.map_err(err)?;
    drop(out);

    let got = format!("{:x}", hasher.finalize());
    if let Some(want) = u.md5_hash.as_deref().filter(|h| !h.is_empty()) {
        if !want.eq_ignore_ascii_case(&got) {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(Stop::Failed(format!("md5 mismatch (expected {want}, got {got})")));
        }
    }
    tokio::fs::rename(&part, &file).await.map_err(err)?;

    let mut path = dir.join(&filename);
    let mut extracted = false;
    if let Some((kind, stem)) = archive_kind(&filename).filter(|_| settings.extract) {
        emit(app, item.game_id, Some(u), received, total, "extracting", None);
        let dest = fs_dir.join(stem);
        let (a, d) = (file.clone(), dest.clone());
        tokio::task::spawn_blocking(move || extract(kind, &a, &d)).await.map_err(err)??;
        if settings.cleanup {
            tokio::fs::remove_file(&file).await.map_err(err)?;
        }
        path = dir.join(stem);
        extracted = true;
    }

    let _guard = st.manifest_lock.lock().await;
    let mut m = state::load_manifest(root);
    m.insert(u.id, ManifestEntry {
        game_id: item.game_id,
        filename: u.filename.clone(),
        md5_hash: u.md5_hash.clone(),
        updated_at: u.updated_at.clone(),
        build_id: u.build_id,
        path,
        extracted,
    });
    state::save_manifest(root, &m)?;
    emit(app, item.game_id, Some(u), received, total, "done", None);
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("Kenney's Pack: Vol 2?"), "Kenney's Pack_ Vol 2_");
        assert_eq!(sanitize("con"), "_con");
        assert_eq!(sanitize("COM1.zip"), "_COM1.zip");
        assert_eq!(sanitize("Console"), "Console");
        assert_eq!(sanitize("trailing. . "), "trailing");
        assert_eq!(sanitize("   "), "_");
    }

    #[test]
    fn safe_join_blocks_escapes() {
        let d = Path::new("/dl/game");
        assert_eq!(safe_join(d, "a/b.png"), Some(PathBuf::from("/dl/game/a/b.png")));
        assert_eq!(safe_join(d, "a\\b.png"), Some(PathBuf::from("/dl/game/a/b.png")));
        assert_eq!(safe_join(d, "./x"), Some(PathBuf::from("/dl/game/x")));
        assert_eq!(safe_join(d, "../evil"), None);
        assert_eq!(safe_join(d, "/etc/passwd"), None);
        assert_eq!(safe_join(d, "./"), None);
    }

    #[test]
    fn extract_zip_and_tar_skip_traversal() {
        let tmp = std::env::temp_dir().join(format!("itchiass-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = tmp.join("a.zip");
        let mut zw = zip::ZipWriter::new(File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("sprites/hero.png", opts).unwrap();
        zw.write_all(b"png").unwrap();
        zw.start_file("../evil.txt", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();
        extract(Kind::Zip, &zip_path, &tmp.join("a")).unwrap();
        assert_eq!(std::fs::read(tmp.join("a/sprites/hero.png")).unwrap(), b"png");
        assert!(!tmp.join("evil.txt").exists());

        let tgz_path = tmp.join("b.tar.gz");
        let gz = flate2::write::GzEncoder::new(File::create(&tgz_path).unwrap(), flate2::Compression::fast());
        let mut tw = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_cksum();
        tw.append_data(&mut h, "dir/tile.png", &b"abc"[..]).unwrap();
        tw.into_inner().unwrap().finish().unwrap();
        extract(Kind::TarGz, &tgz_path, &tmp.join("b")).unwrap();
        assert_eq!(std::fs::read(tmp.join("b/dir/tile.png")).unwrap(), b"abc");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn archive_kinds() {
        assert_eq!(archive_kind("Pack.TAR.GZ"), Some((Kind::TarGz, "Pack")));
        assert_eq!(archive_kind("a.7z"), Some((Kind::SevenZ, "a")));
        assert_eq!(archive_kind("a.rar"), Some((Kind::Rar, "a")));
        assert_eq!(archive_kind("a.png"), None);
    }
}
