use anyhow::{Context, Result};
use serde::Deserialize;
use webrtc::ice_transport::ice_server::RTCIceServer;

/// Cloudflare TURN configuration loaded from environment variables.
#[derive(Clone)]
pub struct TurnConfig {
    token_id: String,
    api_token: String,
}

#[derive(Deserialize)]
struct CfIceServersResponse {
    #[serde(rename = "iceServers")]
    ice_servers: Vec<CfIceServer>,
}

#[derive(Deserialize)]
struct CfIceServer {
    urls: Vec<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    credential: Option<String>,
}

impl TurnConfig {
    /// Read TURN_TOKEN_ID and TURN_API_TOKEN from the environment.
    pub fn from_env() -> Result<Self> {
        let token_id = std::env::var("TURN_TOKEN_ID").context("TURN_TOKEN_ID not set")?;
        let api_token = std::env::var("TURN_API_TOKEN").context("TURN_API_TOKEN not set")?;
        Ok(Self {
            token_id,
            api_token,
        })
    }

    /// Fetch short-lived ICE server credentials from the Cloudflare TURN API.
    pub async fn fetch_ice_servers(&self) -> Result<Vec<RTCIceServer>> {
        let url = format!(
            "https://rtc.live.cloudflare.com/v1/turn/keys/{}/credentials/generate-ice-servers",
            self.token_id
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .json(&serde_json::json!({"ttl": 86400}))
            .send()
            .await
            .context("Failed to contact Cloudflare TURN API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Cloudflare TURN API returned {status}: {body}");
        }

        let parsed: CfIceServersResponse = resp
            .json()
            .await
            .context("Failed to parse Cloudflare TURN API response")?;

        tracing::info!(
            "Fetched {} ICE server entries from Cloudflare",
            parsed.ice_servers.len()
        );

        let servers = parsed
            .ice_servers
            .into_iter()
            .map(|s| RTCIceServer {
                urls: s.urls,
                username: s.username.unwrap_or_default(),
                credential: s.credential.unwrap_or_default(),
                ..Default::default()
            })
            .collect();

        Ok(servers)
    }
}
