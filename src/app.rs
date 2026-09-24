use crate::capture;
use crate::capture::CaptureTarget;
use crate::codec::{H264Decoder, H264Encoder, MfDecoder, MfEncoder, Nv12Frame, VideoDecoder, VideoEncoder};
use crate::diag;
use crate::failure::{SessionFailure, Step};
use crate::signaling_server::SignalingServer;
use crate::transport::control::Control;
use crate::transport::framing::IncomingMessage;
use crate::transport::peer::{self, PeerConnection};
use crate::transport::{CodecInfo, ConnState, SessionStats, SharedConnState};
use crate::viewer::DecodedFrame;
use anyhow::{Context as _, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::RTCPeerConnection;

const FRAME_CHANNEL_SIZE: usize = 4;
/// Captured frames waiting for the encoder. One: when the encoder is behind,
/// a deeper queue only adds latency (each queued frame is a full encode
/// older than the screen) without raising the frame rate.
const CAPTURE_QUEUE_SIZE: usize = 1;

/// How often each side pings the other to measure round-trip time.
const PING_INTERVAL: Duration = Duration::from_secs(2);
/// How long a hang-up waits for its `Bye` to go out before closing anyway.
const BYE_GRACE: Duration = Duration::from_millis(500);
/// Once the `Bye` has left the send buffer, how long to let the transport
/// actually transmit it before tearing the connection down.
const BYE_LINGER: Duration = Duration::from_millis(50);
/// Bounds and cadence of the outgoing-frame still behind the viewer's
/// "you're sharing" indicator.
const PREVIEW_MAX_WIDTH: u32 = 240;
const PREVIEW_MAX_HEIGHT: u32 = 136;
const PREVIEW_INTERVAL: Duration = Duration::from_secs(1);

/// How long a viewer that joins or loses data mid-stream waits, at most, for
/// a picture it can decode from. Timed rather than counted in frames: the
/// capture only delivers frames when the screen changes, so a frame count
/// meant for 60 fps could stretch to tens of seconds on a quiet screen.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

/// A hardware encoder that has taken this many frames without producing any
/// output is treated as broken, and the session falls back to OpenH264; so
/// is a Media Foundation decoder that has taken this many frames since an
/// IDR without producing a picture. Generous: some encoders hold a few
/// frames back before the first comes out.
const HW_FIRST_OUTPUT_WITHIN: u32 = 30;

/// Wakes the GUI. The decoder calls it whenever a frame lands, so the viewer
/// repaints on arrival instead of polling at a fixed rate.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

type DcHolder = Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Offerer,
    Answerer,
}

/// What a session sends: the largest frame the encoder is fed (captures are
/// scaled down to fit, never up), how many frames a second it takes, and
/// whether it may encode and decode on the GPU.
#[derive(Clone, Copy, Debug)]
pub struct Quality {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    /// Try the hardware encoder and Media Foundation's decoder first,
    /// falling back to OpenH264 (the "Automatic" setting). Off means
    /// OpenH264 only, both ways.
    pub hardware_video: bool,
}

/// Where a session has got to in the signaling handshake. Reported to the
/// GUI so the three "please wait" screens can name the step they're on
/// instead of showing three identical spinners.
///
/// The variants are ordered, and the GUI relies on that ordering to mark
/// everything before the current stage as done.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Stage {
    /// Reaching the signaling server: creating the session (offerer) or
    /// looking it up (answerer).
    ContactingServer,
    /// Session exists on the server; waiting on the other side to show up —
    /// their answer (offerer) or their offer (answerer).
    WaitingForPeer,
    /// Both SDPs exchanged; ICE is finding a path and the data channel is
    /// opening.
    Negotiating,
    /// Media threads running, frames about to flow.
    Connected,
}

/// The GUI's handle on a live session: what to draw, and the controls it has
/// over what is being sent. Hanging up goes through the session's cancel
/// channel, the same one the progress screens already use.
#[derive(Clone)]
pub struct SessionHandle {
    pub conn_state: SharedConnState,
    pub stats: Arc<SessionStats>,
    /// The latest decoded remote frame, taken by the viewer when it draws.
    pub frame_buffer: Arc<Mutex<Option<Nv12Frame>>>,
    /// A small, roughly once-a-second copy of what we are sending. Taken from
    /// the encoder's input, so it is right for monitors and windows alike.
    pub outgoing_preview: Arc<Mutex<Option<DecodedFrame>>>,
    paused: Arc<AtomicBool>,
    control_tx: mpsc::UnboundedSender<Control>,
}

impl SessionHandle {
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Stops (or restarts) sending frames, and tells the peer, so their
    /// screen can say why the picture stopped moving.
    pub fn set_paused(&self, paused: bool) {
        if self.paused.swap(paused, Ordering::Relaxed) != paused {
            let msg = if paused { Control::Paused } else { Control::Resumed };
            let _ = self.control_tx.send(msg);
        }
    }
}

pub enum SessionEvent {
    /// The handshake moved on to a new [`Stage`].
    Progress(Stage),
    /// Offerer: session created, here's the code to share.
    SessionCode(String),
    SessionReady(SessionHandle),
    Error(SessionFailure),
}

/// Report a stage transition, ignoring a closed channel: the GUI dropping the
/// receiver means the session is being torn down, which the next real step
/// will surface as `Cancelled` anyway.
fn progress(event_tx: &std::sync::mpsc::Sender<SessionEvent>, stage: Stage) {
    let _ = event_tx.send(SessionEvent::Progress(stage));
}

/// Spawn the offerer session in a background thread. `cancel_rx` lets the GUI
/// abandon the session (e.g. the user hits Cancel or Disconnect) without
/// waiting out the signaling server's poll timeouts.
pub fn spawn_offerer(
    server_url: String,
    target: CaptureTarget,
    quality: Quality,
    custom_pin: Option<String>,
    event_tx: std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
    repaint: Repaint,
) {
    std::thread::Builder::new()
        .name("session-offerer".into())
        .spawn(move || {
            match run_offerer(
                server_url,
                target,
                quality,
                custom_pin.as_deref(),
                &event_tx,
                cancel_rx,
                repaint,
            ) {
                Ok(()) | Err(SessionError::Cancelled) => {}
                Err(SessionError::Other(e)) => fail(&event_tx, &e, Role::Offerer),
            }
        })
        .expect("Failed to spawn offerer thread");
}

/// Spawn the answerer session in a background thread. See `spawn_offerer` for `cancel_rx`.
pub fn spawn_answerer(
    server_url: String,
    target: CaptureTarget,
    quality: Quality,
    session_code: String,
    event_tx: std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
    repaint: Repaint,
) {
    std::thread::Builder::new()
        .name("session-answerer".into())
        .spawn(move || {
            match run_answerer(
                server_url,
                target,
                quality,
                &session_code,
                &event_tx,
                cancel_rx,
                repaint,
            ) {
                Ok(()) | Err(SessionError::Cancelled) => {}
                Err(SessionError::Other(e)) => fail(&event_tx, &e, Role::Answerer),
            }
        })
        .expect("Failed to spawn answerer thread");
}

/// Logs the raw chain (the error screen shows a sentence; the log drawer is
/// where the detail goes) and hands the GUI the classified failure.
fn fail(event_tx: &std::sync::mpsc::Sender<SessionEvent>, error: &anyhow::Error, role: Role) {
    tracing::error!("Session failed: {error:#}");
    let _ = event_tx.send(SessionEvent::Error(SessionFailure::new(error, role)));
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
    target: CaptureTarget,
    quality: Quality,
    custom_pin: Option<&str>,
    event_tx: &std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
    repaint: Repaint,
) -> Result<(), SessionError> {
    let conn_state = SharedConnState::new();
    let stats = Arc::new(SessionStats::default());
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    let rt = tokio::runtime::Runtime::new().map_err(anyhow::Error::from)?;

    if *cancel_rx.borrow() {
        return Err(SessionError::Cancelled);
    }

    let link = rt.block_on(async {
        let server = SignalingServer::new(&server_url);

        progress(event_tx, Stage::ContactingServer);

        // Create session and get ICE servers from the signaling server
        let (code, ice_servers) = server.create_session(custom_pin).await?;
        // Derived from the code both peers share out of band; never sent to
        // the signaling server. See transport::crypto for why this matters.
        let key = crate::transport::crypto::SessionKey::derive(&code);

        let mut peer = PeerConnection::new(conn_state.clone(), ice_servers)
            .await
            .context(Step::PeerSetup)?;
        let offer_json = peer.create_offer().await.context(Step::PeerSetup)?;

        // Upload offer SDP to the signaling server
        server.upload_offer(&code, &offer_json, &key).await?;

        // Tell the GUI the session code so the user can share it
        event_tx
            .send(SessionEvent::SessionCode(code.clone()))
            .map_err(|_| anyhow::anyhow!("GUI disconnected"))?;
        progress(event_tx, Stage::WaitingForPeer);

        // Poll for the answer from the remote peer. Answers that don't
        // decrypt under `key` (i.e. didn't come from someone holding the
        // real code) are silently skipped inside poll_answer. Cancelling
        // here (the "waiting for peer" screen) is the common case this
        // whole plumbing exists for.
        let answer_json = server.poll_answer(&code, &key, cancel_rx.clone()).await?;
        progress(event_tx, Stage::Negotiating);
        peer.set_answer(&answer_json).await.context(Step::PeerSetup)?;

        // Both SDPs are exchanged and every later renegotiation happens
        // in-band, so the server-side session has done its job. Removing it
        // now rather than at app exit is also what lets Reconnect re-create
        // a session under the same code.
        server.delete_session(&code).await;

        let dc_holder = peer.data_channel();
        let incoming_rx = peer.take_incoming_rx().expect("incoming_rx already taken");
        let signaling_tx = peer.signaling_tx();
        let signaling_rx = peer
            .take_signaling_rx()
            .expect("signaling_rx already taken");

        peer::spawn_sender(dc_holder.clone(), encoded_rx, conn_state.clone(), stats.clone());
        peer::spawn_reconnect_monitor(
            peer.rtc_pc(),
            dc_holder.clone(),
            peer.disconnect_notify(),
            signaling_rx,
            conn_state.clone(),
            Role::Offerer,
        );

        anyhow::Ok(Link {
            pc: peer.rtc_pc(),
            dc_holder,
            incoming_rx,
            signaling_tx,
        })
    })?;

    tracing::info!("Offerer signaling complete, starting media threads");
    run_connected(
        &rt, link, target, quality, encoded_tx, conn_state, stats, event_tx, cancel_rx, repaint,
    )
}

fn run_answerer(
    server_url: String,
    target: CaptureTarget,
    quality: Quality,
    session_code: &str,
    event_tx: &std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
    repaint: Repaint,
) -> Result<(), SessionError> {
    let conn_state = SharedConnState::new();
    let stats = Arc::new(SessionStats::default());
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    let rt = tokio::runtime::Runtime::new().map_err(anyhow::Error::from)?;

    if *cancel_rx.borrow() {
        return Err(SessionError::Cancelled);
    }

    let link = rt.block_on(async {
        let server = SignalingServer::new(&server_url);

        // Derived from the code the user was given out of band; never sent
        // to the signaling server. See transport::crypto for why this matters.
        let key = crate::transport::crypto::SessionKey::derive(session_code);

        progress(event_tx, Stage::ContactingServer);

        // Join the session, polling until it exists (handles client starting before server)
        let ice_servers = server
            .join_session_polling(session_code, cancel_rx.clone())
            .await?;

        progress(event_tx, Stage::WaitingForPeer);

        // Poll for the offer from the remote peer. An offer that doesn't
        // decrypt under `key` (wrong code) is silently skipped inside poll_offer.
        let offer_json = server
            .poll_offer(session_code, &key, cancel_rx.clone())
            .await?;
        progress(event_tx, Stage::Negotiating);

        let mut peer = PeerConnection::new(conn_state.clone(), ice_servers)
            .await
            .context(Step::PeerSetup)?;
        let answer_json = peer.accept_offer(&offer_json).await.context(Step::PeerSetup)?;

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

        peer::spawn_sender(dc_holder.clone(), encoded_rx, conn_state.clone(), stats.clone());
        peer::spawn_reconnect_monitor(
            peer.rtc_pc(),
            dc_holder.clone(),
            peer.disconnect_notify(),
            signaling_rx,
            conn_state.clone(),
            Role::Answerer,
        );

        anyhow::Ok(Link {
            pc: peer.rtc_pc(),
            dc_holder,
            incoming_rx,
            signaling_tx,
        })
    })?;

    tracing::info!("Answerer signaling complete, starting media threads");
    run_connected(
        &rt, link, target, quality, encoded_tx, conn_state, stats, event_tx, cancel_rx, repaint,
    )
}

/// What the signaling handshake hands over to the connected phase.
struct Link {
    pc: Arc<RTCPeerConnection>,
    dc_holder: DcHolder,
    incoming_rx: mpsc::Receiver<Vec<u8>>,
    signaling_tx: mpsc::Sender<Vec<u8>>,
}

/// The connected phase, shared by both roles: start the media threads, hand
/// the GUI its [`SessionHandle`], then run the control loop until either side
/// hangs up or the connection dies. Tears everything down afterwards, so a
/// session that has ended stops capturing instead of encoding for nobody
/// until the app exits.
#[allow(clippy::too_many_arguments)]
fn run_connected(
    rt: &tokio::runtime::Runtime,
    link: Link,
    target: CaptureTarget,
    quality: Quality,
    encoded_tx: mpsc::Sender<Vec<u8>>,
    conn_state: SharedConnState,
    stats: Arc<SessionStats>,
    event_tx: &std::sync::mpsc::Sender<SessionEvent>,
    cancel_rx: watch::Receiver<bool>,
    repaint: Repaint,
) -> Result<(), SessionError> {
    let frame_buffer = Arc::new(Mutex::new(None));
    let outgoing_preview = Arc::new(Mutex::new(None));
    let paused = Arc::new(AtomicBool::new(false));
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (peer_control_tx, peer_control_rx) = mpsc::channel(8);

    let capture = match start_media_threads(MediaThreads {
        target,
        quality,
        encoded_tx,
        incoming_rx: link.incoming_rx,
        signaling_tx: link.signaling_tx,
        peer_control_tx,
        frame_buffer: frame_buffer.clone(),
        outgoing_preview: outgoing_preview.clone(),
        paused: paused.clone(),
        conn_state: conn_state.clone(),
        stats: stats.clone(),
        repaint,
    }) {
        Ok(capture) => capture,
        Err(e) => {
            rt.block_on(close_quietly(&link.pc));
            return Err(e.into());
        }
    };

    progress(event_tx, Stage::Connected);
    let handle = SessionHandle {
        conn_state: conn_state.clone(),
        stats: stats.clone(),
        frame_buffer,
        outgoing_preview,
        paused,
        control_tx,
    };
    if event_tx.send(SessionEvent::SessionReady(handle)).is_err() {
        capture.stop();
        rt.block_on(close_quietly(&link.pc));
        return Err(anyhow::anyhow!("GUI disconnected").into());
    }

    rt.block_on(control_loop(
        &link.pc,
        &link.dc_holder,
        &conn_state,
        &stats,
        control_rx,
        peer_control_rx,
        cancel_rx,
    ));

    tracing::info!("Session ended, stopping capture");
    capture.stop();
    Ok(())
}

/// Runs for as long as the session is live: forwards the GUI's pause notices,
/// answers and sends pings, and watches for either side hanging up. Returns
/// once the peer connection has been closed.
async fn control_loop(
    pc: &RTCPeerConnection,
    dc_holder: &DcHolder,
    conn_state: &SharedConnState,
    stats: &SessionStats,
    mut local_rx: mpsc::UnboundedReceiver<Control>,
    mut peer_rx: mpsc::Receiver<Control>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let epoch = tokio::time::Instant::now();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let send = async |msg: Control| {
        // Mid-reconnect the channel is there but nothing gets through;
        // there's no point queueing behind the ICE restart.
        if conn_state.get() != ConnState::Connected {
            return;
        }
        if let Err(e) = peer::send_control(dc_holder, msg).await {
            tracing::debug!("Failed to send {msg:?}: {e}");
        }
    };

    loop {
        tokio::select! {
            // A dropped sender (the GUI went away without saying so) counts
            // as hanging up too.
            _ = cancel_rx.wait_for(|&c| c) => {
                tracing::info!("Hanging up");
                let _ = tokio::time::timeout(BYE_GRACE, async {
                    send(Control::Bye).await;
                    peer::flush(dc_holder, BYE_GRACE).await;
                    // Out of our buffer isn't quite on the wire yet.
                    tokio::time::sleep(BYE_LINGER).await;
                })
                .await;
                break;
            }
            Some(msg) = local_rx.recv() => send(msg).await,
            Some(msg) = peer_rx.recv() => match msg {
                Control::Bye => {
                    tracing::info!("Peer ended the session");
                    conn_state.set_peer_left();
                    break;
                }
                Control::Paused => conn_state.set_peer_paused(true),
                Control::Resumed => conn_state.set_peer_paused(false),
                Control::Ping(n) => send(Control::Pong(n)).await,
                Control::Pong(n) => {
                    // `n` is our own send time, echoed back.
                    let sent_at = Duration::from_micros(n);
                    stats.set_rtt(epoch.elapsed().saturating_sub(sent_at));
                }
            },
            _ = ping.tick() => {
                // The reconnect monitor gave up and closed the connection.
                if conn_state.get() == ConnState::Failed {
                    break;
                }
                send(Control::Ping(epoch.elapsed().as_micros() as u64)).await;
            }
        }
    }

    conn_state.set(ConnState::Failed);
    close_quietly(pc).await;
}

async fn close_quietly(pc: &RTCPeerConnection) {
    if let Err(e) = pc.close().await {
        tracing::warn!("Failed to close peer connection: {e}");
    }
}

struct MediaThreads {
    target: CaptureTarget,
    quality: Quality,
    encoded_tx: mpsc::Sender<Vec<u8>>,
    incoming_rx: mpsc::Receiver<Vec<u8>>,
    signaling_tx: mpsc::Sender<Vec<u8>>,
    peer_control_tx: mpsc::Sender<Control>,
    frame_buffer: Arc<Mutex<Option<Nv12Frame>>>,
    outgoing_preview: Arc<Mutex<Option<DecodedFrame>>>,
    paused: Arc<AtomicBool>,
    conn_state: SharedConnState,
    stats: Arc<SessionStats>,
    repaint: Repaint,
}

/// Starts the encoder and decoder threads, then the capture feeding the
/// encoder. The returned capture is the one thing that needs stopping by
/// hand: the encoder exits when it does, and the decoder when the peer
/// connection's runtime goes away.
fn start_media_threads(threads: MediaThreads) -> Result<capture::session::CaptureSession> {
    let MediaThreads {
        target,
        quality,
        encoded_tx,
        incoming_rx,
        signaling_tx,
        peer_control_tx,
        frame_buffer,
        outgoing_preview,
        paused,
        conn_state,
        stats,
        repaint,
    } = threads;

    diag::start_reporter();

    let (capture_tx, capture_rx) =
        std::sync::mpsc::sync_channel::<capture::Frame>(CAPTURE_QUEUE_SIZE);

    let encoder_stats = stats.clone();
    std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            let mut encoder = EncoderSelector::new(quality, encoder_stats);
            let mut last_preview: Option<Instant> = None;
            let mut last_keyframe: Option<Instant> = None;

            while let Ok(frame) = capture_rx.recv() {
                diag::ENC_IN.inc();
                let (w, h) = (frame.width, frame.height);

                if encoder.prepare(&frame) {
                    last_keyframe = None;
                } else if !encoder.is_ready() {
                    continue;
                }

                if last_preview.is_none_or(|t| t.elapsed() >= PREVIEW_INTERVAL) {
                    last_preview = Some(Instant::now());
                    match frame.read_back() {
                        Ok((y, uv)) => *outgoing_preview.lock().unwrap() = Some(preview_frame(&y, &uv, w, h)),
                        Err(e) => tracing::debug!("Preview readback failed: {e}"),
                    }
                }

                let keyframe_due = last_keyframe.is_some_and(|t| t.elapsed() >= KEYFRAME_INTERVAL);
                let encode_start = Instant::now();
                let result = encoder.encode(&frame, keyframe_due);
                drop(frame);
                let encode_time = encode_start.elapsed();
                diag::ENC_ENCODE_US.add_time(encode_time);
                diag::ENC_ENCODE_MAX_US.max_time(encode_time);
                match result {
                    Ok(h264_data) => {
                        if h264_data.is_empty() {
                            diag::ENC_EMPTY.inc();
                        } else {
                            diag::ENC_OUT.inc();
                            diag::ENC_BYTES.add(h264_data.len() as u64);
                            diag::ENC_MAX_BYTES.max(h264_data.len() as u64);
                            if diag::has_idr(&h264_data) {
                                diag::ENC_IDR.inc();
                                last_keyframe = Some(Instant::now());
                                tracing::debug!("Keyframe: {} KB", h264_data.len() / 1024);
                            }
                            let send_start = Instant::now();
                            let _ = encoded_tx.blocking_send(h264_data);
                            diag::ENC_BLOCKED_US.add_time(send_start.elapsed());
                        }
                    }
                    Err(e) => {
                        diag::ENC_ERR.inc();
                        tracing::warn!("Encode error: {e:#}");
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
            let Some(mut decoder) = DecoderSelector::new(quality.hardware_video, stats.clone()) else {
                return;
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
                    IncomingMessage::Control(msg) => {
                        if peer_control_tx.try_send(msg).is_err() {
                            tracing::warn!("Control channel full or closed, dropping {msg:?}");
                        }
                        continue;
                    }
                    IncomingMessage::Pending => continue,
                };

                diag::DEC_IN.inc();
                let decode_start = Instant::now();
                let result = decoder.decode(&h264_data);
                let decode_time = decode_start.elapsed();
                diag::DEC_US.add_time(decode_time);
                diag::DEC_MAX_US.max_time(decode_time);
                match result {
                    Ok(Some(frame)) => {
                        stats.add_received(h264_data.len());
                        stats.set_remote_size(frame.width, frame.height);
                        diag::DEC_FRAME.inc();
                        let mut slot = fb.lock().unwrap();
                        if slot.is_some() {
                            diag::DEC_OVERWRITTEN.inc();
                        }
                        *slot = Some(frame);
                        drop(slot);
                        repaint();
                    }
                    Ok(None) => diag::DEC_NONE.inc(),
                    Err(e) => {
                        diag::DEC_ERR.inc();
                        tracing::warn!("Decode error: {e}");
                    }
                }
            }
            tracing::info!("Decoder thread exiting");
        })?;

    // Last, so failing to spawn either thread above can't leave a capture
    // running with nothing to stop it.
    capture::session::start_capture(target, capture_tx, paused, quality.fps, quality.max_width, quality.max_height)
        .context(Step::Capture)
}

/// Picks and owns the encoder thread's encoder: the Media Foundation hardware
/// encoder when allowed and it starts, OpenH264 otherwise. If the hardware
/// encoder fails to start, errors, or produces nothing for its first
/// [`HW_FIRST_OUTPUT_WITHIN`] frames, the session falls back to OpenH264 for
/// good: a GPU encoder that failed once is likely to fail again, and each
/// retry would cost the viewer a stall.
struct EncoderSelector {
    fps: u32,
    stats: Arc<SessionStats>,
    /// Whether the session asked for hardware encoding at all.
    hardware_wanted: bool,
    /// Cleared for the rest of the session once the hardware encoder fails.
    hardware_allowed: bool,
    current: Option<ActiveEncoder>,
}

struct ActiveEncoder {
    encoder: Box<dyn VideoEncoder>,
    hardware: bool,
    frames_in: u32,
    produced_output: bool,
}

impl EncoderSelector {
    fn new(quality: Quality, stats: Arc<SessionStats>) -> Self {
        Self {
            fps: quality.fps,
            stats,
            hardware_wanted: quality.hardware_video,
            hardware_allowed: quality.hardware_video,
            current: None,
        }
    }

    fn is_ready(&self) -> bool {
        self.current.is_some()
    }

    /// Makes sure there's an encoder for `frame`'s size, creating one if
    /// needed. Returns whether a new encoder was created.
    fn prepare(&mut self, frame: &capture::Frame) -> bool {
        let (w, h) = (frame.width, frame.height);
        if self.current.as_ref().is_some_and(|c| c.encoder.size() == (w, h)) {
            return false;
        }
        // Release the old encoder (and any frames it holds) before starting
        // a new one: hardware encoders have a session limit.
        self.current = None;

        if self.hardware_allowed {
            match MfEncoder::new(frame.surface.texture(), w, h, self.fps) {
                Ok(e) => {
                    self.install(Box::new(e), true);
                    return true;
                }
                Err(e) => self.disable_hardware(&e),
            }
        }
        match H264Encoder::new(w, h, self.fps) {
            Ok(e) => {
                self.install(Box::new(e), false);
                true
            }
            Err(e) => {
                tracing::error!("Failed to create encoder: {e:#}");
                false
            }
        }
    }

    fn install(&mut self, encoder: Box<dyn VideoEncoder>, hardware: bool) {
        let (w, h) = encoder.size();
        let name = encoder.name().to_string();
        tracing::info!(
            "Encoding {w}x{h} with {name} ({})",
            if hardware { "hardware" } else { "software" }
        );
        diag::set_encoder(&name);
        self.stats.set_local_size(w, h);
        self.stats.set_encoder(CodecInfo {
            hardware,
            fell_back: self.hardware_wanted && !hardware,
        });
        self.current = Some(ActiveEncoder {
            encoder,
            hardware,
            frames_in: 0,
            produced_output: false,
        });
    }

    fn disable_hardware(&mut self, why: &anyhow::Error) {
        tracing::warn!("Hardware encoder unavailable, using OpenH264 for the rest of the session: {why:#}");
        self.hardware_allowed = false;
    }

    /// Encodes `frame` with the current encoder (call [`Self::prepare`]
    /// first). A hardware failure switches to OpenH264 and re-encodes the
    /// same frame, so falling back doesn't cost a frame.
    fn encode(&mut self, frame: &capture::Frame, keyframe_due: bool) -> Result<Vec<u8>> {
        let cur = self.current.as_mut().context("no encoder")?;
        if keyframe_due {
            cur.encoder.force_keyframe();
        }
        let result = cur.encoder.encode(frame);
        if !cur.hardware {
            return result;
        }

        let failure = match result {
            Err(e) => e,
            Ok(data) => {
                cur.frames_in += 1;
                cur.produced_output |= !data.is_empty();
                if cur.produced_output || cur.frames_in < HW_FIRST_OUTPUT_WITHIN {
                    return Ok(data);
                }
                anyhow::anyhow!("{}: no output after {} frames", cur.encoder.name(), cur.frames_in)
            }
        };

        self.disable_hardware(&failure);
        self.current = None;
        if !self.prepare(frame) {
            return Err(failure.context("software fallback failed to start too"));
        }
        self.current.as_mut().context("no encoder")?.encoder.encode(frame)
    }
}

/// Picks and owns the decoder thread's decoder: Media Foundation's H.264
/// decoder (on the GPU where it can) when allowed and it starts, OpenH264
/// otherwise. Like [`EncoderSelector`], a Media Foundation failure (an
/// error, or no picture for [`HW_FIRST_OUTPUT_WITHIN`] frames after an IDR)
/// switches to OpenH264 for the rest of the session. OpenH264 then needs the
/// next IDR (at most [`KEYFRAME_INTERVAL`] away) before it shows anything.
struct DecoderSelector {
    decoder: Box<dyn VideoDecoder>,
    stats: Arc<SessionStats>,
    hardware_wanted: bool,
    /// Whether `decoder` is Media Foundation's, and so may still fall back.
    media_foundation: bool,
    /// Frames fed since the first IDR while no picture has come out yet.
    /// `None` before the first IDR, and once a picture has come out.
    waiting_for_output: Option<u32>,
}

impl DecoderSelector {
    fn new(hardware_wanted: bool, stats: Arc<SessionStats>) -> Option<Self> {
        if hardware_wanted {
            match MfDecoder::new(true) {
                Ok(d) => {
                    let hardware = d.is_hardware();
                    return Some(Self::with(Box::new(d), true, hardware, hardware_wanted, stats));
                }
                Err(e) => tracing::warn!("Media Foundation decoder unavailable, using OpenH264: {e:#}"),
            }
        }
        match H264Decoder::new() {
            Ok(d) => Some(Self::with(Box::new(d), false, false, hardware_wanted, stats)),
            Err(e) => {
                tracing::error!("Failed to create decoder: {e:#}");
                None
            }
        }
    }

    fn with(
        decoder: Box<dyn VideoDecoder>,
        media_foundation: bool,
        hardware: bool,
        hardware_wanted: bool,
        stats: Arc<SessionStats>,
    ) -> Self {
        let selector = Self {
            decoder,
            stats,
            hardware_wanted,
            media_foundation,
            waiting_for_output: None,
        };
        selector.announce(hardware);
        selector
    }

    fn announce(&self, hardware: bool) {
        let name = self.decoder.name();
        tracing::info!("Decoding with {name} ({})", if hardware { "hardware" } else { "software" });
        diag::set_decoder(name);
        self.stats.set_decoder(CodecInfo {
            hardware,
            fell_back: self.hardware_wanted && !hardware,
        });
    }

    fn decode(&mut self, h264: &[u8]) -> Result<Option<Nv12Frame>> {
        let result = self.decoder.decode(h264);
        if !self.media_foundation {
            return result;
        }

        let failure = match result {
            Err(e) => e,
            Ok(Some(frame)) => {
                self.waiting_for_output = None;
                return Ok(Some(frame));
            }
            Ok(None) => {
                if self.waiting_for_output.is_none() && diag::has_idr(h264) {
                    self.waiting_for_output = Some(0);
                }
                let Some(waited) = &mut self.waiting_for_output else {
                    return Ok(None);
                };
                *waited += 1;
                if *waited < HW_FIRST_OUTPUT_WITHIN {
                    return Ok(None);
                }
                anyhow::anyhow!("{}: no picture {waited} frames after an IDR", self.decoder.name())
            }
        };

        tracing::warn!("Media Foundation decoder failed, using OpenH264 for the rest of the session: {failure:#}");
        let openh264 = H264Decoder::new().map_err(|e| failure.context(format!("OpenH264 failed to start too: {e:#}")))?;
        self.decoder = Box::new(openh264);
        self.media_foundation = false;
        self.waiting_for_output = None;
        self.announce(false);
        // Worth a try: this is an IDR often enough (the first frame, say).
        self.decoder.decode(h264)
    }
}

/// Shrinks an encoder-input frame (NV12) to the RGBA still the viewer shows
/// in its sharing indicator.
fn preview_frame(y: &[u8], uv: &[u8], width: u32, height: u32) -> DecodedFrame {
    let (w, h, rgba) = capture::nv12_preview_rgba(y, uv, width, height, PREVIEW_MAX_WIDTH, PREVIEW_MAX_HEIGHT);
    DecodedFrame { width: w, height: h, rgba }
}
