use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::state::err;

// Register at itch.io/user/settings/oauth-apps with redirect http://127.0.0.1/callback.
const CLIENT_ID: &str = match option_env!("ITCHIASS_CLIENT_ID") {
    Some(id) => id,
    None => "92b78b6bcc10b13849db92d2ccca197e",
};
const SCOPE: &str = "profile:me profile:owned game:view:uploads";

fn random_string(n: usize) -> String {
    rand::rng().sample_iter(&Alphanumeric).take(n).map(char::from).collect()
}

pub async fn login(client: &reqwest::Client) -> Result<String, String> {
    let client_id = CLIENT_ID;
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(err)?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(err)?.port());
    let verifier = random_string(64);
    let state = random_string(24);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

    let mut url = Url::parse("https://itch.io/user/oauth").map_err(err)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("scope", SCOPE)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    tauri_plugin_opener::open_url(url.as_str(), None::<&str>).map_err(err)?;

    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_code(&listener, &state))
        .await
        .map_err(|_| "Login timed out".to_string())??;

    let v: Value = client
        .post("https://api.itch.io/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(err)?
        .json()
        .await
        .map_err(err)?;
    v.get("access_token").and_then(Value::as_str).map(String::from).ok_or_else(|| format!("Token exchange failed: {v}"))
}

async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(err)?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let Some(path) = req.split_whitespace().nth(1).filter(|p| p.starts_with("/callback")) else {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        };
        let q: HashMap<String, String> = Url::parse(&format!("http://127.0.0.1{path}")).map_err(err)?.query_pairs().into_owned().collect();
        let body = "<html><body style=\"font-family:sans-serif\"><h2>ItchiAss(ets) is logged in. You can close this tab.</h2></body></html>";
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = sock.write_all(resp.as_bytes()).await;

        if let Some(e) = q.get("error") {
            return Err(format!("itch.io denied login: {e}"));
        }
        if q.get("state").map(String::as_str) != Some(state) {
            return Err("OAuth state mismatch".into());
        }
        return q.get("code").cloned().ok_or_else(|| "No code in callback".into());
    }
}
