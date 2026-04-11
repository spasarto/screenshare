use anyhow::{Context, Result};
use serde::Deserialize;
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

fn to_rtc_servers(entries: Vec<IceServerEntry>) -> Vec<RTCIceServer> {
    entries
        .into_iter()
        .map(|e| RTCIceServer {
            urls: e.urls,
            username: e.username.unwrap_or_default(),
            credential: e.credential.unwrap_or_default(),
            ..Default::default()
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

    /// Create a new session. Optionally pass a custom pin code. Returns (6-digit code, ICE servers).
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

    /// Join an existing session. Returns ICE servers.
    pub async fn join_session(&self, code: &str) -> Result<Vec<RTCIceServer>> {
        let url = format!("{}/api/session/{}", self.base_url, code);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to contact signaling server")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Join session failed ({status}): {body}");
        }

        let parsed: JoinSessionResponse = resp
            .json()
            .await
            .context("Failed to parse join session response")?;

        Ok(to_rtc_servers(parsed.ice_servers))
    }

    /// Upload the offer SDP to the session.
    pub async fn upload_offer(&self, code: &str, sdp_json: &str) -> Result<()> {
        let url = format!("{}/api/session/{}/offer", self.base_url, code);
        let resp = self
            .client
            .put(&url)
            .body(sdp_json.to_string())
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

    /// Upload the answer SDP to the session.
    pub async fn upload_answer(&self, code: &str, sdp_json: &str) -> Result<()> {
        let url = format!("{}/api/session/{}/answer", self.base_url, code);
        let resp = self
            .client
            .put(&url)
            .body(sdp_json.to_string())
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

    /// Poll for the offer SDP until it's available. Returns the offer JSON string.
    pub async fn poll_offer(&self, code: &str) -> Result<String> {
        let url = format!("{}/api/session/{}/offer", self.base_url, code);
        let start = std::time::Instant::now();

        loop {
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
                return parsed
                    .offer
                    .context("Server said ready but no offer present");
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Poll for the answer SDP until it's available. Returns the answer JSON string.
    pub async fn poll_answer(&self, code: &str) -> Result<String> {
        let url = format!("{}/api/session/{}/answer", self.base_url, code);
        let start = std::time::Instant::now();

        loop {
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
                return parsed
                    .answer
                    .context("Server said ready but no answer present");
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
