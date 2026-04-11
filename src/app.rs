use crate::capture;
use crate::codec::{H264Decoder, H264Encoder};
use crate::signaling_server::SignalingServer;
use crate::transport::framing::IncomingMessage;
use crate::transport::peer::{self, PeerConnection};
use crate::transport::{ConnState, SharedConnState};
use crate::viewer::DecodedFrame;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
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
    /// Offerer: session created, here's the 6-digit code to share.
    SessionCode(String),
    SessionReady {
        frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
        conn_state: SharedConnState,
    },
    Error(String),
}

/// Spawn the offerer session in a background thread.
pub fn spawn_offerer(
    server_url: String,
    window: Window,
    custom_pin: Option<String>,
    event_tx: std::sync::mpsc::Sender<SessionEvent>,
) {
    std::thread::Builder::new()
        .name("session-offerer".into())
        .spawn(move || {
            if let Err(e) = run_offerer(server_url, window, custom_pin.as_deref(), &event_tx) {
                let _ = event_tx.send(SessionEvent::Error(format!("{e:#}")));
            }
        })
        .expect("Failed to spawn offerer thread");
}

/// Spawn the answerer session in a background thread.
pub fn spawn_answerer(
    server_url: String,
    window: Window,
    session_code: String,
    event_tx: std::sync::mpsc::Sender<SessionEvent>,
) {
    std::thread::Builder::new()
        .name("session-answerer".into())
        .spawn(move || {
            if let Err(e) = run_answerer(server_url, window, &session_code, &event_tx) {
                let _ = event_tx.send(SessionEvent::Error(format!("{e:#}")));
            }
        })
        .expect("Failed to spawn answerer thread");
}

fn run_offerer(
    server_url: String,
    window: Window,
    custom_pin: Option<&str>,
    event_tx: &std::sync::mpsc::Sender<SessionEvent>,
) -> Result<()> {
    let frame_buffer: Arc<Mutex<Option<DecodedFrame>>> = Arc::new(Mutex::new(None));
    let conn_state = SharedConnState::new();

    let (capture_tx, capture_rx) =
        std::sync::mpsc::sync_channel::<capture::Frame>(FRAME_CHANNEL_SIZE);
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    let _rt = tokio::runtime::Runtime::new()?;

    let (incoming_rx, signaling_tx) = _rt.block_on(async {
        let server = SignalingServer::new(&server_url);

        // Create session and get ICE servers from the signaling server
        let (code, ice_servers) = server.create_session(custom_pin).await?;

        let mut peer = PeerConnection::new(conn_state.clone(), ice_servers).await?;
        let offer_json = peer.create_offer().await?;

        // Upload offer SDP to the signaling server
        server.upload_offer(&code, &offer_json).await?;

        // Tell the GUI the session code so the user can share it
        event_tx
            .send(SessionEvent::SessionCode(code.clone()))
            .map_err(|_| anyhow::anyhow!("GUI disconnected"))?;

        // Poll for the answer from the remote peer
        let answer_json = server.poll_answer(&code).await?;
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
) -> Result<()> {
    let frame_buffer: Arc<Mutex<Option<DecodedFrame>>> = Arc::new(Mutex::new(None));
    let conn_state = SharedConnState::new();

    let (capture_tx, capture_rx) =
        std::sync::mpsc::sync_channel::<capture::Frame>(FRAME_CHANNEL_SIZE);
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    let _rt = tokio::runtime::Runtime::new()?;

    let (incoming_rx, signaling_tx) = _rt.block_on(async {
        let server = SignalingServer::new(&server_url);

        // Join the session and get ICE servers
        let ice_servers = server.join_session(session_code).await?;

        // Poll for the offer from the remote peer
        let offer_json = server.poll_offer(session_code).await?;

        let mut peer = PeerConnection::new(conn_state.clone(), ice_servers).await?;
        let answer_json = peer.accept_offer(&offer_json).await?;

        // Upload our answer to the signaling server
        server.upload_answer(session_code, &answer_json).await?;

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
    capture::wgc::start_capture(window, capture_tx, TARGET_WIDTH, TARGET_HEIGHT)?;

    std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            let mut encoder: Option<H264Encoder> = None;

            while let Ok(frame) = capture_rx.recv() {
                let w = frame.width & !1;
                let h = frame.height & !1;

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
                let expected_size = (w * h * 4) as usize;

                if frame.data.len() != expected_size {
                    tracing::warn!(
                        "Frame size mismatch: got {} expected {} ({}x{})",
                        frame.data.len(),
                        expected_size,
                        w,
                        h
                    );
                    continue;
                }

                match enc.encode(&frame.data) {
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
            decoder_conn_state.set(ConnState::Connected);

            while let Some(raw_msg) = incoming_rx.blocking_recv() {
                let h264_data = match reassembler.feed_classified(&raw_msg) {
                    IncomingMessage::VideoData(data) => data,
                    IncomingMessage::Signaling(json) => {
                        let _ = signaling_tx.blocking_send(json);
                        continue;
                    }
                    IncomingMessage::Pending => continue,
                };

                if decoder_conn_state.get() == ConnState::Reconnecting {
                    reassembler.reset();
                }

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
