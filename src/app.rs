use crate::capture;
use crate::codec::{H264Decoder, H264Encoder};
use crate::signaling_server::SignalingServer;
use crate::transport::framing::IncomingMessage;
use crate::transport::peer::{self, PeerConnection};
use crate::transport::{ConnState, SharedConnState};
use crate::viewer::DecodedFrame;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use windows_capture::window::Window;

const TARGET_WIDTH: u32 = 1280;
const TARGET_HEIGHT: u32 = 720;
const FRAME_CHANNEL_SIZE: usize = 4;

#[derive(Clone, Copy)]
pub enum Role {
    Offerer,
    Answerer,
}

pub enum SessionEvent {
    /// Offerer: session created, here's the code to share.
    SessionCode(String),
    SessionReady {
        frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
        conn_state: SharedConnState,
    },
    Error(String),
}

/// Spawn the offerer session in a background thread. `cancel_rx` lets the GUI
/// abandon the session (e.g. the user hits Cancel) without waiting out the
/// signaling server's poll timeouts.
pub fn spawn_offerer(
    server_url: String,
    window: Window,
    custom_pin: Option<String>,
    event_tx: std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
) {
    std::thread::Builder::new()
        .name("session-offerer".into())
        .spawn(move || {
            match run_offerer(
                server_url,
                window,
                custom_pin.as_deref(),
                &event_tx,
                cancel_rx,
            ) {
                Ok(()) | Err(SessionError::Cancelled) => {}
                Err(SessionError::Other(e)) => {
                    let _ = event_tx.send(SessionEvent::Error(format!("{e:#}")));
                }
            }
        })
        .expect("Failed to spawn offerer thread");
}

/// Spawn the answerer session in a background thread. See `spawn_offerer` for `cancel_rx`.
pub fn spawn_answerer(
    server_url: String,
    window: Window,
    session_code: String,
    event_tx: std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
) {
    std::thread::Builder::new()
        .name("session-answerer".into())
        .spawn(move || {
            match run_answerer(server_url, window, &session_code, &event_tx, cancel_rx) {
                Ok(()) | Err(SessionError::Cancelled) => {}
                Err(SessionError::Other(e)) => {
                    let _ = event_tx.send(SessionEvent::Error(format!("{e:#}")));
                }
            }
        })
        .expect("Failed to spawn answerer thread");
}

/// Distinguishes a user-initiated cancel from a real failure, so a cancelled
/// session doesn't flash an error message on its way back to the setup screen.
enum SessionError {
    Cancelled,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        if e.downcast_ref::<crate::signaling_server::Cancelled>()
            .is_some()
        {
            SessionError::Cancelled
        } else {
            SessionError::Other(e)
        }
    }
}

fn run_offerer(
    server_url: String,
    window: Window,
    custom_pin: Option<&str>,
    event_tx: &std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<(), SessionError> {
    let frame_buffer: Arc<Mutex<Option<DecodedFrame>>> = Arc::new(Mutex::new(None));
    let conn_state = SharedConnState::new();

    let (capture_tx, capture_rx) =
        std::sync::mpsc::sync_channel::<capture::Frame>(FRAME_CHANNEL_SIZE);
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    let _rt = tokio::runtime::Runtime::new().map_err(anyhow::Error::from)?;

    if *cancel_rx.borrow() {
        return Err(SessionError::Cancelled);
    }

    let (incoming_rx, signaling_tx) = _rt.block_on(async {
        let server = SignalingServer::new(&server_url);

        // Create session and get ICE servers from the signaling server
        let (code, ice_servers) = server.create_session(custom_pin).await?;
        // Derived from the code both peers share out of band; never sent to
        // the signaling server. See transport::crypto for why this matters.
        let key = crate::transport::crypto::SessionKey::derive(&code);

        let mut peer = PeerConnection::new(conn_state.clone(), ice_servers).await?;
        let offer_json = peer.create_offer().await?;

        // Upload offer SDP to the signaling server
        server.upload_offer(&code, &offer_json, &key).await?;

        // Tell the GUI the session code so the user can share it
        event_tx
            .send(SessionEvent::SessionCode(code.clone()))
            .map_err(|_| anyhow::anyhow!("GUI disconnected"))?;

        // Poll for the answer from the remote peer. Answers that don't
        // decrypt under `key` (i.e. didn't come from someone holding the
        // real code) are silently skipped inside poll_answer. Cancelling
        // here (the "waiting for peer" screen) is the common case this
        // whole plumbing exists for.
        let answer_json = server.poll_answer(&code, &key, cancel_rx).await?;
        peer.set_answer(&answer_json).await?;

        let dc_holder = peer.data_channel();
        let incoming_rx = peer.take_incoming_rx().expect("incoming_rx already taken");
        let signaling_tx = peer.signaling_tx();
        let signaling_rx = peer
            .take_signaling_rx()
            .expect("signaling_rx already taken");

        peer::spawn_sender(dc_holder.clone(), encoded_rx, conn_state.clone());
        peer::spawn_reconnect_monitor(
            peer.rtc_pc(),
            dc_holder.clone(),
            peer.disconnect_notify(),
            signaling_rx,
            conn_state.clone(),
            Role::Offerer,
        );

        anyhow::Ok((incoming_rx, signaling_tx))
    })?;

    tracing::info!("Offerer signaling complete, starting media threads");
    start_media_threads(
        window,
        capture_tx,
        capture_rx,
        encoded_tx,
        incoming_rx,
        signaling_tx,
        frame_buffer.clone(),
        conn_state.clone(),
    )?;

    event_tx
        .send(SessionEvent::SessionReady {
            frame_buffer,
            conn_state,
        })
        .map_err(|_| anyhow::anyhow!("GUI disconnected"))?;

    // Keep thread alive so the tokio runtime and spawned tasks stay alive
    loop {
        std::thread::park();
    }
}

fn run_answerer(
    server_url: String,
    window: Window,
    session_code: &str,
    event_tx: &std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<(), SessionError> {
    let frame_buffer: Arc<Mutex<Option<DecodedFrame>>> = Arc::new(Mutex::new(None));
    let conn_state = SharedConnState::new();

    let (capture_tx, capture_rx) =
        std::sync::mpsc::sync_channel::<capture::Frame>(FRAME_CHANNEL_SIZE);
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    let _rt = tokio::runtime::Runtime::new().map_err(anyhow::Error::from)?;

    if *cancel_rx.borrow() {
        return Err(SessionError::Cancelled);
    }

    let (incoming_rx, signaling_tx) = _rt.block_on(async {
        let server = SignalingServer::new(&server_url);

        // Derived from the code the user was given out of band; never sent
        // to the signaling server. See transport::crypto for why this matters.
        let key = crate::transport::crypto::SessionKey::derive(session_code);

        // Join the session, polling until it exists (handles client starting before server)
        let ice_servers = server
            .join_session_polling(session_code, cancel_rx.clone())
            .await?;

        // Poll for the offer from the remote peer. An offer that doesn't
        // decrypt under `key` (wrong code) is silently skipped inside poll_offer.
        let offer_json = server.poll_offer(session_code, &key, cancel_rx).await?;

        let mut peer = PeerConnection::new(conn_state.clone(), ice_servers).await?;
        let answer_json = peer.accept_offer(&offer_json).await?;

        // Upload our answer to the signaling server
        server
            .upload_answer(session_code, &answer_json, &key)
            .await?;

        let dc_holder = peer.data_channel();
        let incoming_rx = peer.take_incoming_rx().expect("incoming_rx already taken");
        let signaling_tx = peer.signaling_tx();
        let signaling_rx = peer
            .take_signaling_rx()
            .expect("signaling_rx already taken");

        peer::spawn_sender(dc_holder.clone(), encoded_rx, conn_state.clone());
        peer::spawn_reconnect_monitor(
            peer.rtc_pc(),
            dc_holder.clone(),
            peer.disconnect_notify(),
            signaling_rx,
            conn_state.clone(),
            Role::Answerer,
        );

        anyhow::Ok((incoming_rx, signaling_tx))
    })?;

    tracing::info!("Answerer signaling complete, starting media threads");
    start_media_threads(
        window,
        capture_tx,
        capture_rx,
        encoded_tx,
        incoming_rx,
        signaling_tx,
        frame_buffer.clone(),
        conn_state.clone(),
    )?;

    event_tx
        .send(SessionEvent::SessionReady {
            frame_buffer,
            conn_state,
        })
        .map_err(|_| anyhow::anyhow!("GUI disconnected"))?;

    loop {
        std::thread::park();
    }
}

#[allow(clippy::too_many_arguments)]
fn start_media_threads(
    window: Window,
    capture_tx: std::sync::mpsc::SyncSender<capture::Frame>,
    capture_rx: std::sync::mpsc::Receiver<capture::Frame>,
    encoded_tx: mpsc::Sender<Vec<u8>>,
    incoming_rx: mpsc::Receiver<Vec<u8>>,
    signaling_tx: mpsc::Sender<Vec<u8>>,
    frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
    conn_state: SharedConnState,
) -> Result<()> {
    capture::wgc::start_capture(window, capture_tx)?;

    std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            let mut encoder: Option<H264Encoder> = None;

            while let Ok(frame) = capture_rx.recv() {
                let (w, h, scaled) = capture::scale_bgra(
                    &frame.data,
                    frame.width,
                    frame.height,
                    TARGET_WIDTH,
                    TARGET_HEIGHT,
                );

                if encoder.is_none()
                    || encoder
                        .as_ref()
                        .is_some_and(|e| e.width() != w || e.height() != h)
                {
                    tracing::info!("Creating encoder for {w}x{h}");
                    match H264Encoder::new(w, h) {
                        Ok(e) => encoder = Some(e),
                        Err(e) => {
                            tracing::error!("Failed to create encoder: {e}");
                            continue;
                        }
                    }
                }

                let enc = encoder.as_mut().unwrap();

                match enc.encode(&scaled) {
                    Ok(h264_data) => {
                        if !h264_data.is_empty() {
                            let _ = encoded_tx.blocking_send(h264_data);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Encode error: {e}");
                    }
                }
            }
            tracing::info!("Encoder thread exiting");
        })?;

    let fb = frame_buffer;
    let decoder_conn_state = conn_state;
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            let mut decoder = match H264Decoder::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("Failed to create decoder: {e}");
                    return;
                }
            };

            let mut incoming_rx = incoming_rx;
            let mut reassembler = crate::transport::framing::Reassembler::new();
            // `on_peer_connection_state_change` (peer.rs) is the single source
            // of truth for conn_state; setting Connected here raced it and
            // could stomp a concurrent transition into Reconnecting.
            let mut last_conn_state = decoder_conn_state.get();

            while let Some(raw_msg) = incoming_rx.blocking_recv() {
                // Discard any in-flight partial frame as soon as we notice the
                // connection dropped, rather than only on the next full
                // reassembly (which may never come for the stale data).
                let current_conn_state = decoder_conn_state.get();
                if current_conn_state == ConnState::Reconnecting
                    && last_conn_state != ConnState::Reconnecting
                {
                    reassembler.reset();
                }
                last_conn_state = current_conn_state;

                let h264_data = match reassembler.feed_classified(&raw_msg) {
                    IncomingMessage::VideoData(data) => data,
                    IncomingMessage::Signaling(json) => {
                        if signaling_tx.try_send(json).is_err() {
                            tracing::warn!(
                                "Signaling channel full or closed, dropping in-band message"
                            );
                        }
                        continue;
                    }
                    IncomingMessage::Pending => continue,
                };

                match decoder.decode(&h264_data) {
                    Ok(Some((w, h, rgba))) => {
                        let mut guard = fb.lock().unwrap();
                        *guard = Some(DecodedFrame {
                            width: w,
                            height: h,
                            rgba,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("Decode error: {e}");
                    }
                }
            }
            tracing::info!("Decoder thread exiting");
        })?;

    Ok(())
}
