use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::api::Upload;

pub fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub token: Option<String>,
    pub download_dir: PathBuf,
    pub parallel: usize,
    pub extract: bool,
    pub cleanup: bool,
    pub by_creator: bool,
    pub check_on_launch: bool,
    pub skip_uploads: BTreeSet<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            token: None,
            download_dir: PathBuf::new(),
            parallel: 3,
            extract: true,
            cleanup: true,
            by_creator: true,
            check_on_launch: true,
            skip_uploads: BTreeSet::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(err)?;
        }
        write_atomic(path, &serde_json::to_vec_pretty(self).map_err(err)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ManifestEntry {
    pub game_id: u64,
    pub filename: String,
    pub md5_hash: Option<String>,
    pub updated_at: Option<String>,
    pub build_id: Option<u64>,
    pub path: PathBuf,
    pub extracted: bool,
}

pub type Manifest = BTreeMap<u64, ManifestEntry>;

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(".itchiass.json")
}

pub fn load_manifest(root: &Path) -> Manifest {
    std::fs::read(manifest_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_manifest(root: &Path, m: &Manifest) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(err)?;
    write_atomic(&manifest_path(root), &serde_json::to_vec_pretty(m).map_err(err)?)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    None,
    Downloaded,
    Update,
}

pub fn is_current(u: &Upload, m: &Manifest) -> bool {
    m.get(&u.id).is_some_and(|e| e.updated_at == u.updated_at && e.md5_hash == u.md5_hash && e.build_id == u.build_id)
}

pub fn has_any(game_id: u64, m: &Manifest) -> bool {
    m.values().any(|e| e.game_id == game_id)
}

pub fn status(game_id: u64, uploads: &[Upload], m: &Manifest, skip: &BTreeSet<u64>) -> Status {
    if !has_any(game_id, m) {
        Status::None
    } else if uploads.iter().filter(|u| u.downloadable() && !skip.contains(&u.id)).all(|u| is_current(u, m)) {
        Status::Downloaded
    } else {
        Status::Update
    }
}

pub struct AppState {
    pub client: reqwest::Client,
    pub settings: Mutex<Settings>,
    pub settings_path: PathBuf,
    pub library_cache: PathBuf,
    pub manifest_lock: tokio::sync::Mutex<()>,
    pub sem: Mutex<(usize, Arc<Semaphore>)>,
    pub cancels: Mutex<HashMap<u64, Arc<AtomicBool>>>,
}

impl AppState {
    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn token(&self) -> Result<String, String> {
        self.settings().token.ok_or_else(|| "Not logged in".to_string())
    }

    pub fn cancel_flag(&self, upload_id: u64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(upload_id, flag.clone());
        flag
    }

    pub fn clear_cancel(&self, upload_id: u64) {
        self.cancels.lock().unwrap().remove(&upload_id);
    }

    pub fn semaphore(&self, parallel: usize) -> Arc<Semaphore> {
        let mut s = self.sem.lock().unwrap();
        if s.0 != parallel {
            *s = (parallel, Arc::new(Semaphore::new(parallel.max(1))));
        }
        s.1.clone()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn upload(id: u64, updated_at: &str) -> Upload {
        Upload {
            id,
            filename: format!("{id}.zip"),
            size: None,
            md5_hash: Some("abc".into()),
            updated_at: Some(updated_at.into()),
            build_id: None,
            storage: Some("hosted".into()),
            display_name: None,
        }
    }

    fn entry(game_id: u64, updated_at: &str) -> ManifestEntry {
        ManifestEntry {
            game_id,
            filename: String::new(),
            md5_hash: Some("abc".into()),
            updated_at: Some(updated_at.into()),
            build_id: None,
            path: PathBuf::new(),
            extracted: false,
        }
    }

    #[test]
    fn status_cases() {
        let mut m = Manifest::new();
        let none = BTreeSet::new();
        assert_eq!(status(1, &[upload(10, "a")], &m, &none), Status::None);

        m.insert(10, entry(1, "a"));
        assert_eq!(status(1, &[upload(10, "a")], &m, &none), Status::Downloaded);
        assert_eq!(status(1, &[upload(10, "b")], &m, &none), Status::Update);
        assert_eq!(status(1, &[upload(10, "a"), upload(11, "a")], &m, &none), Status::Update);
        assert_eq!(status(1, &[upload(10, "a"), upload(11, "a")], &m, &BTreeSet::from([11])), Status::Downloaded);

        let mut external = upload(12, "a");
        external.storage = Some("external".into());
        assert_eq!(status(1, &[upload(10, "a"), external], &m, &none), Status::Downloaded);
    }
}
