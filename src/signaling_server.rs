use crate::transport::crypto::SessionKey;
use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::watch;
use webrtc::ice_transport::ice_server::RTCIceServer;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// A request that can't even open a connection in this long isn't going to.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Upper bound on any single request, so a server that accepts the
/// connection and then says nothing can't hang a session forever.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

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

/// The failures the signaling server can report that mean something to the
/// user, as opposed to "the request didn't work". Each one is raised as the
/// root of its `anyhow` chain so [`crate::failure`] can find it with a
/// downcast and turn it into a sentence and a recovery action, while the
/// chain above it keeps the raw detail for the log and the Details view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingError {
    /// 409 on create: a custom pin that another session already holds.
    PinInUse,
    /// 429: the per-IP limit on code-gated endpoints.
    RateLimited,
    /// Joining: the code never appeared on the server within the timeout.
    SessionNotFound,
    /// 404 partway through: the session existed and has since gone
    /// (expired, or the host cancelled).
    SessionGone,
    /// The session exists but the other side never arrived.
    PeerTimedOut,
    /// Something did answer on the session, but never with a blob that
    /// decrypts under the code — a mistyped code on one side, or someone
    /// else trying codes.
    Undecryptable,
    /// Any other non-success status.
    Status(u16),
}

impl std::fmt::Display for SignalingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PinInUse => write!(f, "Pin already in use"),
            Self::RateLimited => write!(f, "Rate limited by the signaling server"),
            Self::SessionNotFound => write!(f, "Timed out waiting for session to be created"),
            Self::SessionGone => write!(f, "Session not found or expired"),
            Self::PeerTimedOut => write!(f, "Timed out waiting for the peer"),
            Self::Undecryptable => write!(f, "Only undecryptable data arrived for this session"),
            Self::Status(code) => write!(f, "Signaling server returned HTTP {code}"),
        }
    }
}

impl std::error::Error for SignalingError {}

/// A non-success response as an error: the status mapped onto
/// [`SignalingError`], with the server's own words kept as context.
async fn status_error(what: &str, resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let kind = match status.as_u16() {
        404 => SignalingError::SessionGone,
        409 => SignalingError::PinInUse,
        429 => SignalingError::RateLimited,
        code => SignalingError::Status(code),
    };
    anyhow::Error::new(kind).context(format!("{what} failed ({status}): {body}"))
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
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
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

        if !resp.status().is_success() {
            return Err(status_error("Create session", resp).await);
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
                anyhow::bail!(SignalingError::SessionNotFound);
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
                return Err(status_error("Join session", resp).await);
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
            return Err(status_error("Upload offer", resp).await);
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
            return Err(status_error("Upload answer", resp).await);
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
        let mut saw_undecryptable = false;

        loop {
            if *cancel.borrow() {
                return Err(Cancelled.into());
            }

            if start.elapsed() > POLL_TIMEOUT {
                if saw_undecryptable {
                    anyhow::bail!(SignalingError::Undecryptable);
                }
                anyhow::bail!(SignalingError::PeerTimedOut);
            }

            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to poll offer")?;

            if !resp.status().is_success() {
                return Err(status_error("Poll offer", resp).await);
            }

            let parsed: PollOfferResponse = resp.json().await?;
            if parsed.ready {
                let ciphertext = parsed
                    .offer
                    .context("Server said ready but no offer present")?;
                match key.decrypt(&ciphertext) {
                    Ok(plaintext) => return Ok(plaintext),
                    Err(e) => {
                        saw_undecryptable = true;
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
        let mut saw_undecryptable = false;

        loop {
            if *cancel.borrow() {
                return Err(Cancelled.into());
            }

            if start.elapsed() > POLL_TIMEOUT {
                if saw_undecryptable {
                    anyhow::bail!(SignalingError::Undecryptable);
                }
                anyhow::bail!(SignalingError::PeerTimedOut);
            }

            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to poll answer")?;

            if !resp.status().is_success() {
                return Err(status_error("Poll answer", resp).await);
            }

            let parsed: PollAnswerResponse = resp.json().await?;
            if parsed.ready {
                let ciphertext = parsed
                    .answer
                    .context("Server said ready but no answer present")?;
                match key.decrypt(&ciphertext) {
                    Ok(plaintext) => return Ok(plaintext),
                    Err(e) => {
                        saw_undecryptable = true;
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

    /// Delete a session from the signaling server. Best effort: a session
    /// left behind expires on the server's own alarm.
    pub async fn delete_session(&self, code: &str) {
        let url = format!("{}/api/session/{}", self.base_url, code);
        if let Err(e) = self.client.delete(&url).send().await {
            tracing::warn!("Failed to delete session {code}: {e}");
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
