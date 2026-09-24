use crate::capture;
use crate::capture::CaptureTarget;
use crate::codec::{H264Decoder, H264Encoder};
use crate::diag;
use crate::failure::{SessionFailure, Step};
use crate::signaling_server::SignalingServer;
use crate::transport::control::Control;
use crate::transport::framing::IncomingMessage;
use crate::transport::peer::{self, PeerConnection};
use crate::transport::{ConnState, SessionStats, SharedConnState};
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
/// scaled down to fit, never up) and how many frames a second it takes.
#[derive(Clone, Copy, Debug)]
pub struct Quality {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
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
    pub frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
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
    frame_buffer: Arc<Mutex<Option<DecodedFrame>>>,
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
            let mut encoder: Option<H264Encoder> = None;
            let mut last_preview: Option<Instant> = None;

            while let Ok(frame) = capture_rx.recv() {
                diag::ENC_IN.inc();
                let (w, h) = (frame.width, frame.height);

                if encoder.is_none()
                    || encoder
                        .as_ref()
                        .is_some_and(|e| e.width() != w || e.height() != h)
                {
                    tracing::info!("Creating encoder for {w}x{h}");
                    match H264Encoder::new(w, h, quality.fps) {
                        Ok(e) => {
                            encoder = Some(e);
                            encoder_stats.set_local_size(w, h);
                        }
                        Err(e) => {
                            tracing::error!("Failed to create encoder: {e}");
                            continue;
                        }
                    }
                }

                if last_preview.is_none_or(|t| t.elapsed() >= PREVIEW_INTERVAL) {
                    last_preview = Some(Instant::now());
                    *outgoing_preview.lock().unwrap() = Some(preview_frame(&frame.y, &frame.uv, w, h));
                }

                let enc = encoder.as_mut().unwrap();

                let encode_start = Instant::now();
                let result = enc.encode(&frame.y, &frame.uv);
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
                                tracing::debug!("Keyframe: {} KB", h264_data.len() / 1024);
                            }
                            let send_start = Instant::now();
                            let _ = encoded_tx.blocking_send(h264_data);
                            diag::ENC_BLOCKED_US.add_time(send_start.elapsed());
                        }
                    }
                    Err(e) => {
                        diag::ENC_ERR.inc();
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
                    Ok(Some((w, h, rgba))) => {
                        stats.add_received(h264_data.len());
                        stats.set_remote_size(w, h);
                        diag::DEC_FRAME.inc();
                        let mut slot = fb.lock().unwrap();
                        if slot.is_some() {
                            diag::DEC_OVERWRITTEN.inc();
                        }
                        *slot = Some(DecodedFrame {
                            width: w,
                            height: h,
                            rgba,
                        });
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

/// Shrinks an encoder-input frame (NV12) to the RGBA still the viewer shows
/// in its sharing indicator.
fn preview_frame(y: &[u8], uv: &[u8], width: u32, height: u32) -> DecodedFrame {
    let (w, h, rgba) = capture::nv12_preview_rgba(y, uv, width, height, PREVIEW_MAX_WIDTH, PREVIEW_MAX_HEIGHT);
    DecodedFrame { width: w, height: h, rgba }
}
