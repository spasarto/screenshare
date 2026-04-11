use crate::crypto::Crypto;
use crate::transport::signaling;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;

pub struct PeerConnection {
    pc: Arc<RTCPeerConnection>,
    data_channel: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    incoming_tx: mpsc::Sender<Vec<u8>>,
    incoming_rx: Option<mpsc::Receiver<Vec<u8>>>,
}

impl PeerConnection {
    pub async fn new() -> Result<Self> {
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
            ice_servers: vec![
                RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_string()],
                    ..Default::default()
                },
                RTCIceServer {
                    urls: vec!["stun:stun1.l.google.com:19302".to_string()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let pc = api.new_peer_connection(config).await?;
        let pc = Arc::new(pc);

        // Channel for incoming data from remote peer
        let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(30);

        // Shared data channel reference (set when channel is created or received)
        let dc_holder: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        // Monitor connection state
        let pc2 = Arc::downgrade(&pc);
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let pc2 = pc2.clone();
            Box::pin(async move {
                tracing::info!("Connection state: {state}");
                if state == RTCPeerConnectionState::Failed {
                    if let Some(pc) = pc2.upgrade() {
                        if let Err(err) = pc.close().await {
                            tracing::warn!("Failed to close peer connection after failure: {err}");
                        }
                    }
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
        })
    }

    /// Attach an on_message handler to a data channel
    fn setup_on_message(dc: &Arc<RTCDataChannel>, tx: mpsc::Sender<Vec<u8>>) {
        let tx = tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(msg.data.to_vec()).await;
            })
        }));
    }

    /// Take the incoming data receiver (can only be called once)
    pub fn take_incoming_rx(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.incoming_rx.take()
    }

    /// Create an offer (offerer role). Returns the encrypted SDP blob.
    pub async fn create_offer(&self, crypto: &Crypto) -> Result<String> {
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

        signaling::encode_blob(&local_desc, crypto)
    }

    /// Accept an offer and create an answer (answerer role).
    pub async fn accept_offer(&self, offer_blob: &str, crypto: &Crypto) -> Result<String> {
        let offer = signaling::decode_blob(offer_blob, crypto)?;
        self.pc.set_remote_description(offer).await?;

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer).await?;

        self.wait_for_ice_gathering().await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .context("No local description")?;

        signaling::encode_blob(&local_desc, crypto)
    }

    /// Complete the connection (offerer sets the answer).
    pub async fn set_answer(&self, answer_blob: &str, crypto: &Crypto) -> Result<()> {
        let answer = signaling::decode_blob(answer_blob, crypto)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Get a clone of the data channel holder for sending frames.
    pub fn data_channel(&self) -> Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> {
        self.data_channel.clone()
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
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), gather_rx.recv()).await;
    }
}

/// Spawn a task that reads encoded frames and sends them over the data channel
/// with chunked framing to handle messages larger than SCTP max size.
pub fn spawn_sender(
    dc_holder: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    mut outgoing_rx: mpsc::Receiver<Vec<u8>>,
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
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        tracing::info!("Data channel ready, starting to send frames");

        while let Some(data) = outgoing_rx.recv().await {
            let dc_guard = dc_holder.lock().await;
            if let Some(dc) = dc_guard.as_ref() {
                let chunks = super::framing::encode_chunked(&data);
                for chunk in chunks {
                    if let Err(e) = dc.send(&bytes::Bytes::from(chunk)).await {
                        tracing::warn!("Failed to send on data channel: {e}");
                        break;
                    }
                }
            }
        }
    })
}
