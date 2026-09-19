use crate::app::Role;
use crate::diag;
use crate::transport::signaling::{self, SignalingBlob};
use crate::transport::control::Control;
use crate::transport::{ConnState, SessionStats, SharedConnState};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::stats::StatsReportType;

pub const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const BACKOFF_BASE_MS: u64 = 1000;
const BACKOFF_CAP_MS: u64 = 30000;

/// Above this much unsent data queued on the SCTP send buffer, drop the next
/// frame instead of adding to it. Without a ceiling the queue grows without
/// bound on a link slower than the encoder's bitrate, and the viewer falls
/// permanently behind rather than recovering.
const MAX_BUFFERED_AMOUNT: usize = 262_144; // 256 KiB

pub struct PeerConnection {
    pc: Arc<RTCPeerConnection>,
    data_channel: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    incoming_tx: mpsc::Sender<Vec<u8>>,
    incoming_rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// Notified when the connection enters the Disconnected state.
    disconnect_notify: Arc<Notify>,
    /// Channel for receiving in-band signaling messages (SDP for ICE restart).
    signaling_tx: mpsc::Sender<Vec<u8>>,
    signaling_rx: Option<mpsc::Receiver<Vec<u8>>>,
}

impl PeerConnection {
    pub async fn new(conn_state: SharedConnState, ice_servers: Vec<RTCIceServer>) -> Result<Self> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("Failed to register codecs")?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .context("Failed to register interceptors")?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let pc = api.new_peer_connection(config).await?;
        let pc = Arc::new(pc);

        // Channel for incoming data from remote peer
        let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(30);

        // Channel for in-band signaling messages (ICE restart SDP exchange)
        let (signaling_tx, signaling_rx) = mpsc::channel::<Vec<u8>>(4);

        // Shared data channel reference (set when channel is created or received)
        let dc_holder: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let disconnect_notify = Arc::new(Notify::new());

        // Monitor connection state
        let state = conn_state.clone();
        let notify = disconnect_notify.clone();
        let pc2 = Arc::downgrade(&pc);
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let state = state.clone();
            let notify = notify.clone();
            let pc2 = pc2.clone();
            Box::pin(async move {
                tracing::info!("Connection state: {s}");
                match s {
                    RTCPeerConnectionState::Connected => {
                        state.set(ConnState::Connected);
                        // Query ICE stats to determine connection type
                        if let Some(pc) = pc2.upgrade() {
                            let stats = pc.get_stats().await;
                            let mut local_type = String::new();
                            let mut remote_type = String::new();
                            // Find the nominated candidate pair
                            let mut local_id = String::new();
                            let mut remote_id = String::new();
                            for report in stats.reports.values() {
                                if let StatsReportType::CandidatePair(pair) = report {
                                    if pair.nominated {
                                        local_id = pair.local_candidate_id.clone();
                                        remote_id = pair.remote_candidate_id.clone();
                                        break;
                                    }
                                }
                            }
                            if !local_id.is_empty() {
                                for report in stats.reports.values() {
                                    match report {
                                        StatsReportType::LocalCandidate(c) if c.id == local_id => {
                                            local_type = format!("{}", c.candidate_type);
                                        }
                                        StatsReportType::RemoteCandidate(c) if c.id == remote_id => {
                                            remote_type = format!("{}", c.candidate_type);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            let relay = local_type == "relay" || remote_type == "relay";
                            tracing::info!(
                                "ICE selected pair: local={local_type} remote={remote_type} relay={relay}"
                            );
                            state.set_ice_info(super::IceInfo {
                                local_type,
                                relay,
                            });
                        }
                    }
                    RTCPeerConnectionState::Disconnected => {
                        state.set(ConnState::Reconnecting);
                        notify.notify_one();
                    }
                    RTCPeerConnectionState::Failed => {
                        // If we were already reconnecting, the monitor task handles retries.
                        // If we jump straight to Failed without Disconnected, signal the monitor.
                        if state.get() != ConnState::Reconnecting {
                            state.set(ConnState::Reconnecting);
                            notify.notify_one();
                        }
                    }
                    RTCPeerConnectionState::Closed => {
                        state.set(ConnState::Failed);
                        // Drop the weak ref - nothing to do
                        let _ = pc2;
                    }
                    _ => {}
                }
            })
        }));

        // Handle incoming data channels (answerer side receives the offerer's channel)
        let dc_holder2 = dc_holder.clone();
        let incoming_tx2 = incoming_tx.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            tracing::info!("Data channel opened by remote: {}", dc.label());
            let dc_holder = dc_holder2.clone();
            let incoming_tx = incoming_tx2.clone();
            Box::pin(async move {
                // Store the channel so the answerer can also send on it
                {
                    let mut holder = dc_holder.lock().await;
                    *holder = Some(dc.clone());
                }
                // Set up receive handler
                Self::setup_on_message(&dc, incoming_tx);
            })
        }));

        Ok(Self {
            pc,
            data_channel: dc_holder,
            incoming_tx,
            incoming_rx: Some(incoming_rx),
            disconnect_notify,
            signaling_tx,
            signaling_rx: Some(signaling_rx),
        })
    }

    /// Attach an on_message handler to a data channel
    fn setup_on_message(dc: &Arc<RTCDataChannel>, tx: mpsc::Sender<Vec<u8>>) {
        let tx = tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                diag::RCV_MSGS.inc();
                diag::RCV_BYTES.add(msg.data.len() as u64);
                // Time spent waiting here is time the SCTP stack can't
                // deliver the next message: the decoder isn't keeping up.
                let wait_start = std::time::Instant::now();
                let _ = tx.send(msg.data.to_vec()).await;
                diag::RCV_QUEUE_WAIT_US.add_time(wait_start.elapsed());
            })
        }));
    }

    /// Take the incoming data receiver (can only be called once)
    pub fn take_incoming_rx(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.incoming_rx.take()
    }

    /// Take the signaling receiver for in-band SDP messages (can only be called once)
    pub fn take_signaling_rx(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.signaling_rx.take()
    }

    /// Get the signaling sender for routing signaling messages from the decoder dispatcher.
    pub fn signaling_tx(&self) -> mpsc::Sender<Vec<u8>> {
        self.signaling_tx.clone()
    }

    /// Create an offer (offerer role). Returns the SDP as a JSON string.
    pub async fn create_offer(&self) -> Result<String> {
        // Create data channel (offerer creates it, answerer receives it via on_data_channel)
        let dc = self
            .pc
            .create_data_channel("video", None)
            .await
            .context("Failed to create data channel")?;

        // Store the data channel for sending
        {
            let mut holder = self.data_channel.lock().await;
            *holder = Some(dc.clone());
        }

        // Set up on_message so the offerer can RECEIVE frames from the answerer
        // (data channels are bidirectional)
        Self::setup_on_message(&dc, self.incoming_tx.clone());

        // Set up on_open to log readiness
        let dc2 = dc.clone();
        dc.on_open(Box::new(move || {
            tracing::info!("Data channel '{}' opened", dc2.label());
            Box::pin(async {})
        }));

        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer).await?;

        // Wait for ICE gathering to complete
        self.wait_for_ice_gathering().await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .context("No local description")?;

        signaling::encode_sdp_json(&local_desc)
    }

    /// Accept an offer and create an answer (answerer role).
    pub async fn accept_offer(&self, offer_json: &str) -> Result<String> {
        let offer = signaling::decode_sdp_json(offer_json)?;
        self.pc.set_remote_description(offer).await?;

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer).await?;

        self.wait_for_ice_gathering().await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .context("No local description")?;

        signaling::encode_sdp_json(&local_desc)
    }

    /// Complete the connection (offerer sets the answer).
    pub async fn set_answer(&self, answer_json: &str) -> Result<()> {
        let answer = signaling::decode_sdp_json(answer_json)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Get a clone of the data channel holder for sending frames.
    pub fn data_channel(&self) -> Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> {
        self.data_channel.clone()
    }

    /// Get the underlying RTCPeerConnection (for ICE restart).
    pub fn rtc_pc(&self) -> Arc<RTCPeerConnection> {
        self.pc.clone()
    }

    /// Get the disconnect notifier.
    pub fn disconnect_notify(&self) -> Arc<Notify> {
        self.disconnect_notify.clone()
    }

    async fn wait_for_ice_gathering(&self) {
        let (gather_tx, mut gather_rx) = mpsc::channel(1);
        self.pc
            .on_ice_gathering_state_change(Box::new(move |state| {
                let tx = gather_tx.clone();
                Box::pin(async move {
                    if state
                        == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete
                    {
                        let _ = tx.send(()).await;
                    }
                })
            }));
        let _ = tokio::time::timeout(Duration::from_secs(10), gather_rx.recv()).await;
    }
}

/// Wait for ICE gathering to complete on a raw RTCPeerConnection reference.
async fn wait_for_ice_gathering_raw(pc: &RTCPeerConnection) {
    let (gather_tx, mut gather_rx) = mpsc::channel(1);
    pc.on_ice_gathering_state_change(Box::new(move |state| {
        let tx = gather_tx.clone();
        Box::pin(async move {
            if state == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete {
                let _ = tx.send(()).await;
            }
        })
    }));
    let _ = tokio::time::timeout(Duration::from_secs(10), gather_rx.recv()).await;
}

/// Spawn a task that reads encoded frames and sends them over the data channel
/// with chunked framing to handle messages larger than SCTP max size.
pub fn spawn_sender(
    dc_holder: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    mut outgoing_rx: mpsc::Receiver<Vec<u8>>,
    conn_state: SharedConnState,
    stats: Arc<SessionStats>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Wait for the data channel to become available before sending.
        // On the answerer side, the DC arrives via on_data_channel asynchronously
        // and may not be ready when the encoder starts producing frames.
        loop {
            let dc_guard = dc_holder.lock().await;
            if dc_guard.is_some() {
                break;
            }
            drop(dc_guard);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tracing::info!("Data channel ready, starting to send frames");

        while let Some(data) = outgoing_rx.recv().await {
            diag::SND_IN.inc();

            // Drop frames while reconnecting
            if conn_state.get() != ConnState::Connected {
                diag::SND_DROP_STATE.inc();
                continue;
            }

            // Clone the channel handle and release the holder lock before
            // sending. Holding it across a whole frame's chunks previously
            // blocked `send_raw`'s ICE-restart signaling (which needs the
            // same lock) for as long as this send took.
            let dc = {
                let dc_guard = dc_holder.lock().await;
                dc_guard.clone()
            };
            let Some(dc) = dc else {
                diag::SND_DROP_NO_DC.inc();
                continue;
            };

            // The link can't keep up with the encoder; drop this frame
            // rather than let the SCTP send queue grow without bound.
            let buffered = dc.buffered_amount().await;
            diag::SND_MAX_BUFFERED.max(buffered as u64);
            if buffered > MAX_BUFFERED_AMOUNT {
                diag::SND_DROP_BUFFERED.inc();
                tracing::debug!(
                    "Dropping {} KB frame: {} KB already buffered",
                    data.len() / 1024,
                    buffered / 1024
                );
                continue;
            }

            let send_start = std::time::Instant::now();

            let chunks = super::framing::encode_chunked(&data);
            let mut sent = true;
            for chunk in chunks {
                if let Err(e) = dc.send(&bytes::Bytes::from(chunk)).await {
                    diag::SND_ERR.inc();
                    tracing::warn!("Failed to send on data channel: {e}");
                    sent = false;
                    break;
                }
            }
            let send_time = send_start.elapsed();
            diag::SND_SEND_US.add_time(send_time);
            diag::SND_SEND_MAX_US.max_time(send_time);
            if sent {
                diag::SND_SENT.inc();
                stats.add_sent(data.len());
            }
        }
    })
}

/// Drop any signaling messages still sitting in the channel from a previous,
/// abandoned reconnect attempt so the next attempt doesn't consume them.
fn drain_stale_signaling(signaling_rx: &mut mpsc::Receiver<Vec<u8>>) {
    let mut dropped = 0u32;
    while signaling_rx.try_recv().is_ok() {
        dropped += 1;
    }
    if dropped > 0 {
        tracing::warn!(
            "Reconnect monitor: dropped {dropped} stale signaling message(s) \
             left over from a previous attempt"
        );
    }
}

/// Wait for a signaling blob tagged with `generation`, discarding anything
/// else (a straggler from an earlier attempt) until `timeout` elapses.
async fn recv_matching_generation(
    signaling_rx: &mut mpsc::Receiver<Vec<u8>>,
    generation: u32,
    timeout: Duration,
    what: &str,
) -> Result<SignalingBlob> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::anyhow!("Timed out waiting for {what}"));
        }
        let json = tokio::time::timeout(remaining, signaling_rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("Timed out waiting for {what}"))?
            .context("Signaling channel closed")?;
        let blob = SignalingBlob::from_json(&json)?;
        if blob.generation != generation {
            tracing::warn!(
                "Reconnect monitor: discarding stale {what} (generation {} != expected {generation})",
                blob.generation
            );
            continue;
        }
        return Ok(blob);
    }
}

/// Send a raw framed message over the data channel.
async fn send_raw(
    dc_holder: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    data: Vec<u8>,
) -> Result<()> {
    let dc_guard = dc_holder.lock().await;
    let dc = dc_guard
        .as_ref()
        .context("Data channel not available for signaling")?;
    dc.send(&bytes::Bytes::from(data))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send signaling message: {e}"))?;
    Ok(())
}

/// Send a session control message to the peer. Best effort by nature: a
/// control message that can't be delivered (no channel yet, link down) has
/// nothing useful to retry against.
pub async fn send_control(
    dc_holder: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    control: Control,
) -> Result<()> {
    send_raw(dc_holder, control.encode()).await
}

/// Waits (up to `timeout`) for everything queued on the data channel to be
/// handed to the network. `send` only queues, so closing the peer connection
/// straight after one otherwise discards the message.
pub async fn flush(
    dc_holder: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    timeout: Duration,
) {
    let Some(dc) = dc_holder.lock().await.clone() else {
        return;
    };
    let _ = tokio::time::timeout(timeout, async {
        while dc.buffered_amount().await > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// Spawn the reconnection monitor task. This task waits for disconnection notifications
/// and performs ICE restart negotiation over the existing data channel.
pub fn spawn_reconnect_monitor(
    pc: Arc<RTCPeerConnection>,
    dc_holder: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    disconnect_notify: Arc<Notify>,
    mut signaling_rx: mpsc::Receiver<Vec<u8>>,
    conn_state: SharedConnState,
    role: Role,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut attempt = 0u32;

        loop {
            // Wait for a disconnection signal
            disconnect_notify.notified().await;
            // Closed on purpose (either side hung up): nothing to restart.
            if conn_state.get() == ConnState::Failed {
                tracing::info!("Reconnect monitor: connection closed, stopping");
                break;
            }
            tracing::info!("Reconnect monitor: disconnection detected, starting ICE restart");

            attempt += 1;
            if attempt > MAX_RECONNECT_ATTEMPTS {
                tracing::error!(
                    "Reconnect monitor: exhausted {MAX_RECONNECT_ATTEMPTS} attempts, giving up"
                );
                conn_state.set(ConnState::Failed);
                if let Err(e) = pc.close().await {
                    tracing::warn!("Failed to close peer connection: {e}");
                }
                break;
            }
            conn_state.set_reconnect_attempt(attempt);

            // Exponential backoff
            let delay = BACKOFF_BASE_MS * 2u64.pow(attempt - 1);
            let delay = delay.min(BACKOFF_CAP_MS);
            tracing::info!(
                "Reconnect monitor: attempt {attempt}/{MAX_RECONNECT_ATTEMPTS}, \
                 waiting {delay}ms before ICE restart"
            );
            tokio::time::sleep(Duration::from_millis(delay)).await;

            // Check if we reconnected while waiting
            if conn_state.get() == ConnState::Connected {
                tracing::info!("Reconnect monitor: connection restored during backoff");
                attempt = 0;
                continue;
            }

            // A message left over from a previous, abandoned attempt must
            // not be mistaken for part of this one.
            drain_stale_signaling(&mut signaling_rx);

            let result = match role {
                Role::Offerer => {
                    ice_restart_offerer(&pc, &dc_holder, &mut signaling_rx, attempt).await
                }
                Role::Answerer => ice_restart_answerer(&pc, &dc_holder, &mut signaling_rx).await,
            };

            match result {
                Ok(()) => {
                    tracing::info!("Reconnect monitor: ICE restart signaling complete");
                    // Wait briefly for the connection to come back up
                    let recovered = tokio::time::timeout(Duration::from_secs(15), async {
                        loop {
                            if conn_state.get() == ConnState::Connected {
                                return true;
                            }
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    })
                    .await;

                    match recovered {
                        Ok(true) => {
                            tracing::info!("Reconnect monitor: connection recovered");
                            attempt = 0;
                        }
                        _ => {
                            tracing::warn!(
                                "Reconnect monitor: ICE restart signaling succeeded \
                                 but connection did not recover"
                            );
                            // Will retry on next loop iteration if still disconnected
                            if conn_state.get() != ConnState::Connected {
                                conn_state.set(ConnState::Reconnecting);
                                disconnect_notify.notify_one();
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Reconnect monitor: ICE restart failed: {e}");
                    // Signal ourselves to retry
                    if conn_state.get() != ConnState::Connected {
                        conn_state.set(ConnState::Reconnecting);
                        disconnect_notify.notify_one();
                    }
                }
            }
        }
    })
}

/// Offerer: initiate ICE restart, send new offer over DC, wait for answer.
async fn ice_restart_offerer(
    pc: &Arc<RTCPeerConnection>,
    dc_holder: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    signaling_rx: &mut mpsc::Receiver<Vec<u8>>,
    generation: u32,
) -> Result<()> {
    pc.restart_ice()
        .await
        .map_err(|e| anyhow::anyhow!("restart_ice failed: {e}"))?;

    let offer = pc
        .create_offer(Some(RTCOfferOptions {
            ice_restart: true,
            ..Default::default()
        }))
        .await
        .map_err(|e| anyhow::anyhow!("create_offer (ICE restart) failed: {e}"))?;

    pc.set_local_description(offer)
        .await
        .map_err(|e| anyhow::anyhow!("set_local_description failed: {e}"))?;

    wait_for_ice_gathering_raw(pc).await;

    let local_desc = pc
        .local_description()
        .await
        .context("No local description after ICE restart")?;

    // Send the offer over the data channel as a signaling frame
    let blob = SignalingBlob::from_sdp(&local_desc, generation);
    let json = blob.to_json()?;
    let frame = super::framing::encode_signaling(&json);
    send_raw(dc_holder, frame).await?;

    tracing::info!("Reconnect monitor (offerer): sent ICE restart offer, waiting for answer");

    // Wait for the answer matching this attempt, ignoring anything left over
    // from an earlier one that slipped in after the drain above.
    let answer_blob = recv_matching_generation(
        signaling_rx,
        generation,
        Duration::from_secs(15),
        "ICE restart answer",
    )
    .await?;
    let answer_sdp = answer_blob.into_sdp()?;

    pc.set_remote_description(answer_sdp)
        .await
        .map_err(|e| anyhow::anyhow!("set_remote_description (answer) failed: {e}"))?;

    Ok(())
}

/// Answerer: wait for ICE restart offer over DC, create answer, send it back.
async fn ice_restart_answerer(
    pc: &Arc<RTCPeerConnection>,
    dc_holder: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    signaling_rx: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    tracing::info!("Reconnect monitor (answerer): waiting for ICE restart offer");

    // The caller already drained anything left over from a previous
    // attempt, so the first offer to arrive here belongs to this one.
    let offer_json = tokio::time::timeout(Duration::from_secs(30), signaling_rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for ICE restart offer"))?
        .context("Signaling channel closed")?;

    let offer_blob = SignalingBlob::from_json(&offer_json)?;
    let generation = offer_blob.generation;
    let offer_sdp = offer_blob.into_sdp()?;

    pc.set_remote_description(offer_sdp)
        .await
        .map_err(|e| anyhow::anyhow!("set_remote_description (offer) failed: {e}"))?;

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| anyhow::anyhow!("create_answer failed: {e}"))?;

    pc.set_local_description(answer)
        .await
        .map_err(|e| anyhow::anyhow!("set_local_description failed: {e}"))?;

    wait_for_ice_gathering_raw(pc).await;

    let local_desc = pc
        .local_description()
        .await
        .context("No local description after creating answer")?;

    // Send the answer back over the data channel, tagged with the same
    // generation as the offer it responds to.
    let blob = SignalingBlob::from_sdp(&local_desc, generation);
    let json = blob.to_json()?;
    let frame = super::framing::encode_signaling(&json);
    send_raw(dc_holder, frame).await?;

    tracing::info!("Reconnect monitor (answerer): sent ICE restart answer");

    Ok(())
}
