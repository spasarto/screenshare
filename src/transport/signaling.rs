use anyhow::{Context, Result};
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

    /// Serialize to JSON bytes (for in-band signaling over an already-encrypted data channel).
    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("Failed to serialize signaling blob")
    }

    /// Deserialize from JSON bytes.
    pub fn from_json(data: &[u8]) -> Result<Self> {
        serde_json::from_slice(data).context("Failed to deserialize signaling blob")
    }

    /// Convert to an RTCSessionDescription.
    pub fn into_sdp(self) -> Result<RTCSessionDescription> {
        blob_to_sdp(self)
    }
}

/// Encode an SDP to a JSON string (for server-relayed signaling).
pub fn encode_sdp_json(sdp: &RTCSessionDescription) -> Result<String> {
    let blob = SignalingBlob::from_sdp(sdp);
    serde_json::to_string(&blob).context("Failed to serialize SDP to JSON")
}

/// Decode an SDP from a JSON string (for server-relayed signaling).
pub fn decode_sdp_json(json: &str) -> Result<RTCSessionDescription> {
    let blob: SignalingBlob =
        serde_json::from_str(json).context("Failed to deserialize SDP from JSON")?;
    blob_to_sdp(blob)
}

fn blob_to_sdp(blob: SignalingBlob) -> Result<RTCSessionDescription> {
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
