mod api;
mod downloads;
mod oauth;
mod state;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Semaphore;

use downloads::QueueItem;
use state::{AppState, Settings, Status, err};

#[derive(Serialize)]
struct SettingsView {
    logged_in: bool,
    download_dir: PathBuf,
    parallel: usize,
    extract: bool,
    cleanup: bool,
    by_creator: bool,
    check_on_launch: bool,
}

#[derive(Deserialize)]
struct SettingsInput {
    download_dir: PathBuf,
    parallel: usize,
    extract: bool,
    cleanup: bool,
    by_creator: bool,
    check_on_launch: bool,
}

#[derive(Serialize, Deserialize)]
struct LibraryGame {
    game_id: u64,
    key_id: u64,
    title: String,
    creator: String,
    classification: String,
    cover_url: Option<String>,
    url: Option<String>,
    platforms: Vec<String>,
    status: Status,
}

#[derive(Serialize)]
struct UploadView {
    id: u64,
    filename: String,
    display_name: Option<String>,
    size: Option<u64>,
    downloaded: bool,
    skipped: bool,
}

fn persist(st: &AppState, f: impl FnOnce(&mut Settings)) -> Result<(), String> {
    let mut s = st.settings.lock().unwrap();
    f(&mut s);
    s.save(&st.settings_path)
}

#[tauri::command]
fn get_settings(st: State<AppState>) -> SettingsView {
    let s = st.settings();
    SettingsView {
        logged_in: s.token.is_some(),
        download_dir: s.download_dir,
        parallel: s.parallel,
        extract: s.extract,
        cleanup: s.cleanup,
        by_creator: s.by_creator,
        check_on_launch: s.check_on_launch,
    }
}

#[tauri::command]
fn save_settings(st: State<AppState>, input: SettingsInput) -> Result<(), String> {
    persist(&st, |s| {
        s.download_dir = input.download_dir;
        s.parallel = input.parallel.clamp(1, 16);
        s.extract = input.extract;
        s.cleanup = input.cleanup;
        s.by_creator = input.by_creator;
        s.check_on_launch = input.check_on_launch;
    })
}

#[tauri::command]
async fn login(st: State<'_, AppState>) -> Result<(), String> {
    let token = oauth::login(&st.client).await?;
    persist(&st, |s| s.token = Some(token))
}

#[tauri::command]
async fn set_api_key(st: State<'_, AppState>, key: String) -> Result<(), String> {
    let key = key.trim().to_string();
    api::verify(&st.client, &key).await?;
    persist(&st, |s| s.token = Some(key))
}

#[tauri::command]
fn logout(st: State<AppState>) -> Result<(), String> {
    let _ = std::fs::remove_file(&st.library_cache);
    persist(&st, |s| s.token = None)
}

fn with_status(mut games: Vec<LibraryGame>, st: &AppState) -> Vec<LibraryGame> {
    let manifest = state::load_manifest(&st.settings().download_dir);
    for g in &mut games {
        g.status = if state::has_any(g.game_id, &manifest) { Status::Downloaded } else { Status::None };
    }
    games
}

#[tauri::command]
fn cached_library(st: State<AppState>) -> Vec<LibraryGame> {
    let games = std::fs::read(&st.library_cache).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    with_status(games, &st)
}

#[tauri::command]
async fn library(st: State<'_, AppState>) -> Result<Vec<LibraryGame>, String> {
    let token = st.token()?;
    let keys = api::owned_keys(&st.client, &token).await?;
    let mut seen = HashSet::new();
    let games: Vec<LibraryGame> = keys
        .into_iter()
        .filter(|k| seen.insert(k.game.id))
        .map(|k| {
            let g = k.game;
            let platforms = [(g.p_windows, "windows"), (g.p_linux, "linux"), (g.p_osx, "mac"), (g.p_android, "android")]
                .into_iter()
                .filter_map(|(on, name)| on.then(|| name.to_string()))
                .collect();
            let creator = g.user.map(|u| u.display_name.filter(|d| !d.is_empty()).unwrap_or(u.username)).unwrap_or_default();
            LibraryGame {
                game_id: g.id,
                key_id: k.id,
                status: Status::None,
                title: g.title,
                creator,
                classification: g.classification.unwrap_or_else(|| "other".into()),
                cover_url: g.cover_url,
                url: g.url,
                platforms,
            }
        })
        .collect();
    if let Some(dir) = st.library_cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&st.library_cache, serde_json::to_vec(&games).map_err(err)?);
    Ok(with_status(games, &st))
}

#[tauri::command]
async fn check_updates(st: State<'_, AppState>, items: Vec<(u64, u64)>) -> Result<HashMap<u64, Status>, String> {
    let token = st.token()?;
    let settings = st.settings();
    let manifest = state::load_manifest(&settings.download_dir);
    let (client, token, manifest, skip) = (&st.client, &token, &manifest, &settings.skip_uploads);
    let results: Vec<(u64, Status)> = futures_util::stream::iter(items)
        .map(|(game_id, key_id)| async move {
            // A failed lookup keeps the game as downloaded rather than failing the whole check.
            let s = match api::uploads(client, token, game_id, key_id).await {
                Ok(u) => state::status(game_id, &u, manifest, skip),
                Err(_) => Status::Downloaded,
            };
            (game_id, s)
        })
        .buffer_unordered(8)
        .collect()
        .await;
    Ok(results.into_iter().collect())
}

#[tauri::command]
async fn uploads(st: State<'_, AppState>, game_id: u64, key_id: u64) -> Result<Vec<UploadView>, String> {
    let token = st.token()?;
    let settings = st.settings();
    let manifest = state::load_manifest(&settings.download_dir);
    let list = api::uploads(&st.client, &token, game_id, key_id).await?;
    Ok(list
        .into_iter()
        .filter(|u| u.downloadable())
        .map(|u| UploadView {
            downloaded: state::is_current(&u, &manifest),
            skipped: settings.skip_uploads.contains(&u.id),
            id: u.id,
            filename: u.filename,
            display_name: u.display_name.filter(|d| !d.is_empty()),
            size: u.size,
        })
        .collect())
}

#[tauri::command]
fn set_skipped(st: State<AppState>, upload_id: u64, skipped: bool) -> Result<(), String> {
    persist(&st, |s| {
        if skipped {
            s.skip_uploads.insert(upload_id);
        } else {
            s.skip_uploads.remove(&upload_id);
        }
    })
}

#[tauri::command]
fn download(app: AppHandle, items: Vec<QueueItem>) {
    downloads::enqueue(app, items);
}

#[tauri::command]
fn cancel(st: State<AppState>, upload_ids: Vec<u64>) {
    let flags = st.cancels.lock().unwrap();
    for id in upload_ids {
        if let Some(f) = flags.get(&id) {
            f.store(true, Ordering::Relaxed);
        }
    }
}

#[tauri::command]
fn open_folder(st: State<AppState>, game_id: u64) -> Result<(), String> {
    let manifest = state::load_manifest(&st.settings().download_dir);
    let path = manifest
        .values()
        .filter(|e| e.game_id == game_id)
        .map(|e| e.path.clone())
        .find(|p| p.exists())
        .ok_or("The downloaded files were moved or deleted")?;
    tauri_plugin_opener::reveal_item_in_dir(path).map_err(err)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            let mut settings = Settings::load(&settings_path);
            if settings.download_dir.as_os_str().is_empty() {
                settings.download_dir = app.path().download_dir()?.join("ItchiAss");
            }
            let parallel = settings.parallel;
            app.manage(AppState {
                client: reqwest::Client::builder().user_agent("ItchiAss/0.1").build()?,
                settings: Mutex::new(settings),
                settings_path,
                library_cache: app.path().app_cache_dir()?.join("library.json"),
                manifest_lock: tokio::sync::Mutex::new(()),
                sem: Mutex::new((parallel, Arc::new(Semaphore::new(parallel.max(1))))),
                cancels: Mutex::new(HashMap::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            login,
            set_api_key,
            logout,
            cached_library,
            library,
            check_updates,
            uploads,
            set_skipped,
            download,
            cancel,
            open_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
