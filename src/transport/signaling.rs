use crate::crypto::Crypto;
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

#[derive(Serialize, Deserialize)]
pub struct SignalingBlob {
    pub sdp: String,
    pub sdp_type: String,
}

impl SignalingBlob {
    pub fn from_sdp(sdp: &RTCSessionDescription) -> Self {
        Self {
            sdp: sdp.sdp.clone(),
            sdp_type: sdp.sdp_type.to_string(),
        }
    }
}

pub fn encode_blob(sdp: &RTCSessionDescription, crypto: &Crypto) -> Result<String> {
    let blob = SignalingBlob::from_sdp(sdp);
    let json = serde_json::to_vec(&blob).context("Failed to serialize SDP")?;
    let encrypted = crypto.encrypt(&json)?;
    Ok(BASE64.encode(&encrypted))
}

pub fn decode_blob(encoded: &str, crypto: &Crypto) -> Result<RTCSessionDescription> {
    let encrypted = BASE64
        .decode(encoded.trim())
        .context("Invalid base64 in signaling blob")?;
    let json = crypto.decrypt(&encrypted)?;
    let blob: SignalingBlob =
        serde_json::from_slice(&json).context("Failed to deserialize SDP")?;

    let sdp_type = match blob.sdp_type.as_str() {
        "offer" => webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Offer,
        "answer" => webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Answer,
        "pranswer" => webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Pranswer,
        "rollback" => webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Rollback,
        _ => return Err(anyhow::anyhow!("Unknown SDP type: {}", blob.sdp_type)),
    };

    let desc = match sdp_type {
        webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Offer => {
            RTCSessionDescription::offer(blob.sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse offer SDP: {e}"))?
        }
        webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Answer => {
            RTCSessionDescription::answer(blob.sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse answer SDP: {e}"))?
        }
        _ => {
            // For pranswer/rollback, use serde deserialization which handles private fields
            let json = serde_json::json!({
                "type": blob.sdp_type,
                "sdp": blob.sdp,
            });
            serde_json::from_value(json).context("Failed to construct RTCSessionDescription")?
        }
    };

    Ok(desc)
}
