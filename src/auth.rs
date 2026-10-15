use crate::{model::Region, storage::Credentials};
use anyhow::{Context, Result, bail};
use rand::Rng;
use serde_json::Value;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc::UnboundedSender,
};

pub const REDIRECT: &str = "http://localhost:18741/callback";

pub fn authorize_url(client_id: &str, state: &str, region: Region) -> String {
    let mut url = reqwest::Url::parse(&format!("{}/oauth", region.web())).unwrap();
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("state", state)
        .append_pair("redirect_uri", REDIRECT);
    url.into()
}

pub async fn login(
    credentials: Credentials,
    region: Region,
    urls: UnboundedSender<String>,
) -> Result<String> {
    if credentials.client_id.trim().is_empty() || credentials.client_secret.trim().is_empty() {
        bail!("Paste the app Client ID and Client secret first");
    }
    // Bind before opening the browser: a fast callback must not race the listener.
    let listener = TcpListener::bind(("127.0.0.1", 18741))
        .await
        .context("Cannot start sign-in: localhost port 18741 is already in use")?;
    let state: String = rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let url = authorize_url(&credentials.client_id, &state, region);
    let _ = urls.send(url.clone());
    let _ = crate::platform::open_url(&url);
    let code = tokio::time::timeout(Duration::from_secs(150), callback(listener, &state))
        .await
        .context("Sign-in timed out. Try again.")??;
    exchange(&credentials, &code, region).await
}

async fn callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = Vec::new();
        let request = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut chunk = [0u8; 1024];
                let count = socket.read(&mut chunk).await?;
                if count == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > 16_384 {
                    bail!("Request too large");
                }
                if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        if !matches!(request, Ok(Ok(()))) {
            continue;
        }
        let request = String::from_utf8_lossy(&buffer);
        let mut parts = request.lines().next().unwrap_or("").split_whitespace();
        if parts.next() != Some("GET") {
            continue;
        }
        let target = parts.next().unwrap_or("");
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
            continue;
        };
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let valid = url.path() == "/callback"
            && query
                .get("state")
                .is_some_and(|state| state == expected_state);
        let body = if valid {
            "Authorization received. Return to Intercom TUI to finish signing in."
        } else {
            "Invalid sign-in callback. Return to the original authorization page."
        };
        let status = if valid { "200 OK" } else { "400 Bad Request" };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            socket.write_all(response.as_bytes()),
        )
        .await;
        if !valid {
            continue;
        } // Forged callbacks cannot cancel a legitimate login.
        if query.contains_key("error") {
            bail!("Intercom authorization was declined or cancelled");
        }
        let code = query
            .get("code")
            .filter(|code| !code.is_empty())
            .context("Intercom did not return an authorization code")?;
        return Ok(code.clone());
    }
}

async fn exchange(credentials: &Credentials, code: &str, preferred: Region) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    for region in preferred.order() {
        let response = client
            .post(format!("{}/auth/eagle/token", region.api()))
            .header("Accept", "application/json")
            .form(&[
                ("code", code),
                ("client_id", credentials.client_id.as_str()),
                ("client_secret", credentials.client_secret.as_str()),
            ])
            .send()
            .await
            .context("Cannot reach Intercom to complete sign-in")?;
        let status = response.status();
        if [401, 403, 404].contains(&status.as_u16()) {
            continue;
        }
        if !status.is_success() {
            bail!("Intercom could not complete sign-in ({status})");
        }
        let payload: Value = response
            .json()
            .await
            .context("Invalid Intercom sign-in response")?;
        let token = payload["token"]
            .as_str()
            .or_else(|| payload["access_token"].as_str())
            .filter(|s| !s.trim().is_empty())
            .context("Intercom did not return an access token")?;
        return Ok(token.into());
    }
    bail!("Intercom rejected the authorization code or app credentials. Try signing in again.")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oauth_url_encodes_and_preserves_callback() {
        let url = reqwest::Url::parse(&authorize_url("id&x", "random state", Region::Eu)).unwrap();
        assert_eq!(url.host_str(), Some("app.eu.intercom.com"));
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "id&x");
        assert_eq!(query["state"], "random state");
        assert_eq!(query["redirect_uri"], REDIRECT);
    }
    #[tokio::test]
    async fn rejects_forged_state_then_accepts_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { callback(listener, "valid").await });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{address}/callback?state=forged&code=bad"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        client
            .get(format!("http://{address}/callback?state=valid&code=good"))
            .send()
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), "good");
    }
}
