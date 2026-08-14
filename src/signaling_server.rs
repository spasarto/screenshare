use crate::transport::crypto::SessionKey;
use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::watch;
use webrtc::ice_transport::ice_server::RTCIceServer;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub struct SignalingServer {
    base_url: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct CreateSessionResponse {
    code: String,
    #[serde(rename = "iceServers")]
    ice_servers: Vec<IceServerEntry>,
}

#[derive(Deserialize)]
struct JoinSessionResponse {
    #[serde(rename = "iceServers")]
    ice_servers: Vec<IceServerEntry>,
}

#[derive(Deserialize)]
struct IceServerEntry {
    urls: Vec<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    credential: Option<String>,
}

#[derive(Deserialize)]
struct PollOfferResponse {
    ready: bool,
    offer: Option<String>,
}

#[derive(Deserialize)]
struct PollAnswerResponse {
    ready: bool,
    answer: Option<String>,
}

/// Marks a poll loop bailing out because the caller cancelled it, as opposed
/// to a real failure. Callers can tell the two apart with `downcast_ref`
/// instead of matching on an error message.
#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Sleep for `POLL_INTERVAL`, waking early if `cancel` is flipped to `true`.
/// Returns an error in that case so callers can bail out of their poll loop.
async fn sleep_or_cancelled(cancel: &mut watch::Receiver<bool>) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(POLL_INTERVAL) => Ok(()),
        _ = cancel.wait_for(|&c| c) => Err(Cancelled.into()),
    }
}

fn to_rtc_servers(entries: Vec<IceServerEntry>) -> Vec<RTCIceServer> {
    entries
        .into_iter()
        .map(|e| RTCIceServer {
            urls: e.urls,
            username: e.username.unwrap_or_default(),
            credential: e.credential.unwrap_or_default(),
        })
        .collect()
}

impl SignalingServer {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create a new session. Optionally pass a custom pin code. Returns (session code, ICE servers).
    pub async fn create_session(
        &self,
        custom_pin: Option<&str>,
    ) -> Result<(String, Vec<RTCIceServer>)> {
        let url = format!("{}/api/session", self.base_url);

        let mut req = self.client.post(&url);
        if let Some(pin) = custom_pin {
            req = req
                .header("Content-Type", "application/json")
                .body(format!(r#"{{"code":"{}"}}"#, pin));
        }

        let resp = req
            .send()
            .await
            .context("Failed to contact signaling server")?;

        if resp.status().as_u16() == 409 {
            anyhow::bail!("Pin already in use");
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Create session failed ({status}): {body}");
        }

        let parsed: CreateSessionResponse = resp
            .json()
            .await
            .context("Failed to parse create session response")?;

        Ok((parsed.code, to_rtc_servers(parsed.ice_servers)))
    }

    /// Join a session, polling until it appears on the signaling server.
    /// This handles the case where the client launches before the server has created
    /// the session — retries on 404 until the session exists or the timeout elapses.
    pub async fn join_session_polling(
        &self,
        code: &str,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Vec<RTCIceServer>> {
        let url = format!("{}/api/session/{}", self.base_url, code);
        let start = std::time::Instant::now();

        loop {
            if *cancel.borrow() {
                return Err(Cancelled.into());
            }

            if start.elapsed() > POLL_TIMEOUT {
                anyhow::bail!("Timed out waiting for session to be created");
            }

            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to contact signaling server")?;

            if resp.status().as_u16() == 404 {
                tracing::info!("Session {code} not found yet, retrying...");
                sleep_or_cancelled(&mut cancel).await?;
                continue;
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Join session failed ({status}): {body}");
            }

            let parsed: JoinSessionResponse = resp
                .json()
                .await
                .context("Failed to parse join session response")?;

            return Ok(to_rtc_servers(parsed.ice_servers));
        }
    }

    /// Upload the offer SDP to the session, encrypted under `key`.
    pub async fn upload_offer(&self, code: &str, sdp_json: &str, key: &SessionKey) -> Result<()> {
        let ciphertext = key.encrypt(sdp_json).context("Failed to encrypt offer")?;
        let url = format!("{}/api/session/{}/offer", self.base_url, code);
        let resp = self
            .client
            .put(&url)
            .body(ciphertext)
            .send()
            .await
            .context("Failed to upload offer")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Upload offer failed ({status}): {body}");
        }
        Ok(())
    }

    /// Upload the answer SDP to the session, encrypted under `key`.
    pub async fn upload_answer(&self, code: &str, sdp_json: &str, key: &SessionKey) -> Result<()> {
        let ciphertext = key.encrypt(sdp_json).context("Failed to encrypt answer")?;
        let url = format!("{}/api/session/{}/answer", self.base_url, code);
        let resp = self
            .client
            .put(&url)
            .body(ciphertext)
            .send()
            .await
            .context("Failed to upload answer")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Upload answer failed ({status}): {body}");
        }
        Ok(())
    }

    /// Poll for the offer SDP until it's available and decrypts under `key`.
    /// Returns the offer JSON string. A blob that fails to decrypt (wrong
    /// code, or forged by someone who doesn't have it) is treated the same
    /// as "not ready yet" rather than being handed to the caller.
    pub async fn poll_offer(
        &self,
        code: &str,
        key: &SessionKey,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<String> {
        let url = format!("{}/api/session/{}/offer", self.base_url, code);
        let start = std::time::Instant::now();

        loop {
            if *cancel.borrow() {
                return Err(Cancelled.into());
            }

            if start.elapsed() > POLL_TIMEOUT {
                anyhow::bail!("Timed out waiting for offer");
            }

            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to poll offer")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Poll offer failed ({status}): {body}");
            }

            let parsed: PollOfferResponse = resp.json().await?;
            if parsed.ready {
                let ciphertext = parsed
                    .offer
                    .context("Server said ready but no offer present")?;
                match key.decrypt(&ciphertext) {
                    Ok(plaintext) => return Ok(plaintext),
                    Err(e) => {
                        tracing::warn!(
                            "Offer for session {code} failed to decrypt ({e:#}); ignoring"
                        );
                    }
                }
            }

            sleep_or_cancelled(&mut cancel).await?;
        }
    }

    /// Poll for the answer SDP until it's available and decrypts under `key`.
    /// Returns the answer JSON string. A blob that fails to decrypt (wrong
    /// code, or forged by someone who doesn't have it) is treated the same
    /// as "not ready yet" rather than being applied.
    pub async fn poll_answer(
        &self,
        code: &str,
        key: &SessionKey,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<String> {
        let url = format!("{}/api/session/{}/answer", self.base_url, code);
        let start = std::time::Instant::now();

        loop {
            if *cancel.borrow() {
                return Err(Cancelled.into());
            }

            if start.elapsed() > POLL_TIMEOUT {
                anyhow::bail!("Timed out waiting for answer");
            }

            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to poll answer")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Poll answer failed ({status}): {body}");
            }

            let parsed: PollAnswerResponse = resp.json().await?;
            if parsed.ready {
                let ciphertext = parsed
                    .answer
                    .context("Server said ready but no answer present")?;
                match key.decrypt(&ciphertext) {
                    Ok(plaintext) => return Ok(plaintext),
                    Err(e) => {
                        tracing::warn!(
                            "Answer for session {code} failed to decrypt ({e:#}); ignoring \
                             (this is expected if someone else is probing this code)"
                        );
                    }
                }
            }

            sleep_or_cancelled(&mut cancel).await?;
        }
    }

    /// Delete a session from the signaling server (blocking, for use on app exit).
    pub fn delete_session_blocking(&self, code: &str) {
        let url = format!("{}/api/session/{}", self.base_url, code);
        if let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
        {
            let _ = client.delete(&url).send();
        }
    }
}
