use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::state::err;

const API: &str = "https://api.itch.io";

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub struct User {
    pub username: String,
    pub display_name: Option<String>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub struct Game {
    pub id: u64,
    pub title: String,
    pub classification: Option<String>,
    pub cover_url: Option<String>,
    pub url: Option<String>,
    pub user: Option<User>,
    pub p_windows: bool,
    pub p_linux: bool,
    pub p_osx: bool,
    pub p_android: bool,
}

#[derive(Deserialize, Clone)]
pub struct OwnedKey {
    pub id: u64,
    pub game: Game,
}

#[derive(Deserialize, Clone)]
pub struct Upload {
    pub id: u64,
    #[serde(default)]
    pub filename: String,
    pub size: Option<u64>,
    pub md5_hash: Option<String>,
    pub updated_at: Option<String>,
    pub build_id: Option<u64>,
    pub storage: Option<String>,
    pub display_name: Option<String>,
}

impl Upload {
    pub fn downloadable(&self) -> bool {
        self.storage.as_deref() != Some("external")
    }
}

// itch encodes empty arrays as {} and sometimes sends lists as id-keyed objects.
fn list<'de, D: serde::Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let items = match Value::deserialize(d)? {
        Value::Array(a) => a,
        Value::Object(o) => o.into_iter().map(|(_, v)| v).collect(),
        _ => Vec::new(),
    };
    items.into_iter().map(serde_json::from_value).collect::<Result<_, _>>().map_err(serde::de::Error::custom)
}

// itch returns HTTP 200 with an "errors" array on failure.
fn check(v: Value) -> Result<Value, String> {
    match v.get("errors").and_then(Value::as_array) {
        Some(errs) => Err(errs.iter().map(|e| e.as_str().unwrap_or("?")).collect::<Vec<_>>().join(", ")),
        None => Ok(v),
    }
}

// Retries rate limits, server errors and dropped connections with exponential backoff.
async fn send(req: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
    const TRIES: u32 = 5;
    for attempt in 1..=TRIES {
        let wait = match req.try_clone().ok_or("request not retryable")?.send().await {
            Ok(resp) if resp.status() == 429 || resp.status().is_server_error() => {
                if attempt == TRIES {
                    return resp.error_for_status().map_err(err);
                }
                let after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
                Duration::from_secs(after.unwrap_or(1 << attempt).min(60))
            }
            Ok(resp) => return resp.error_for_status().map_err(err),
            Err(e) if attempt < TRIES && (e.is_connect() || e.is_timeout() || e.is_request()) => Duration::from_secs(1 << attempt),
            Err(e) => return Err(e.to_string()),
        };
        tokio::time::sleep(wait).await;
    }
    unreachable!()
}

async fn get<T: DeserializeOwned>(client: &reqwest::Client, token: &str, path: &str, query: &[(&str, String)]) -> Result<T, String> {
    let v: Value = send(client.get(format!("{API}{path}")).bearer_auth(token).query(query)).await?.json().await.map_err(err)?;
    serde_json::from_value(check(v)?).map_err(err)
}

pub async fn verify(client: &reqwest::Client, token: &str) -> Result<(), String> {
    get::<Value>(client, token, "/profile", &[]).await.map(|_| ())
}

pub async fn owned_keys(client: &reqwest::Client, token: &str) -> Result<Vec<OwnedKey>, String> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default, deserialize_with = "list")]
        owned_keys: Vec<OwnedKey>,
    }
    let mut all = Vec::new();
    for page in 1.. {
        let p: Page = get(client, token, "/profile/owned-keys", &[("page", page.to_string()), ("per_page", "500".into())]).await?;
        let repeat = p.owned_keys.first().is_some_and(|k| all.iter().any(|a: &OwnedKey| a.id == k.id));
        if p.owned_keys.is_empty() || repeat {
            break;
        }
        all.extend(p.owned_keys);
    }
    Ok(all)
}

pub async fn uploads(client: &reqwest::Client, token: &str, game_id: u64, key_id: u64) -> Result<Vec<Upload>, String> {
    #[derive(Deserialize)]
    struct R {
        #[serde(default, deserialize_with = "list")]
        uploads: Vec<Upload>,
    }
    let r: R = get(client, token, &format!("/games/{game_id}/uploads"), &[("download_key_id", key_id.to_string())]).await?;
    Ok(r.uploads)
}

fn ranged(req: reqwest::RequestBuilder, from: u64) -> reqwest::RequestBuilder {
    if from > 0 { req.header(reqwest::header::RANGE, format!("bytes={from}-")) } else { req }
}

pub async fn download(client: &reqwest::Client, token: &str, upload_id: u64, key_id: u64, from: u64) -> Result<reqwest::Response, String> {
    let req = client.get(format!("{API}/uploads/{upload_id}/download")).bearer_auth(token).query(&[("download_key_id", key_id.to_string())]);
    let resp = send(ranged(req, from)).await?;
    let is_json = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/json"));
    if !is_json {
        return Ok(resp);
    }
    let v = check(resp.json().await.map_err(err)?)?;
    let url = v.get("url").and_then(Value::as_str).ok_or("Download returned no file or url")?;
    send(ranged(client.get(url), from)).await
}


#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct R {
        #[serde(default, deserialize_with = "list")]
        uploads: Vec<Upload>,
    }

    #[test]
    fn empty_object_is_empty_list() {
        let r: R = serde_json::from_str(r#"{"uploads":{}}"#).unwrap();
        assert!(r.uploads.is_empty());
        let r: R = serde_json::from_str(r#"{"uploads":[{"id":1}]}"#).unwrap();
        assert_eq!(r.uploads[0].id, 1);
    }
}
