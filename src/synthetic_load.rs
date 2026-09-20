//! Debug-only data-channel throughput check.
//!
//! Phase 0 of `docs/hardware-encoding-plan.md`: nothing has pushed the
//! webrtc-rs SCTP data channel past ~3 Mbit/s yet, and a hardware encoder at
//! 1440p60 will want 8-15 Mbit/s. This runs the real signaling handshake and
//! peer connection, then feeds [`peer::spawn_sender`] fixed-size frames at a
//! fixed rate instead of capturing and encoding, so the channel's own limits
//! show up in the `pipeline | send` diag log line independent of encode cost.
//!
//! Run two local instances against each other, one with `--server
//! --synthetic-kb N --synthetic-fps N`, the other with `--client --pin
//! <code printed by the first> --synthetic-kb N --synthetic-fps N`, and read
//! `send avg`, `drop_buffered` and `max buffered` once connected.

use crate::app::Role;
use crate::diag;
use crate::signaling_server::SignalingServer;
use crate::transport::peer::{self, PeerConnection};
use crate::transport::{ConnState, SessionStats, SharedConnState};
use anyhow::{Context as _, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// How long to wait for the peer connection to reach `Connected` before
/// giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct SyntheticLoadArgs {
    pub server_url: String,
    pub role: Role,
    /// Offerer: a custom pin, or `None` to let the server generate one.
    /// Answerer: the session code to join (required).
    pub pin: Option<String>,
    pub frame_kb: u32,
    pub fps: u32,
    pub duration: Duration,
}

pub fn run(args: SyntheticLoadArgs) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().context("Failed to start Tokio runtime")?;
    rt.block_on(run_async(args))
}

async fn run_async(args: SyntheticLoadArgs) -> Result<()> {
    diag::start_reporter();

    let conn_state = SharedConnState::new();
    let stats = Arc::new(SessionStats::default());
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(4);
    // Nothing ever cancels this run early; it either connects within
    // CONNECT_TIMEOUT or gives up.
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let server = SignalingServer::new(&args.server_url);

    let (pc, dc_holder, mut incoming_rx) = match args.role {
        Role::Offerer => {
            let (code, ice_servers) = server.create_session(args.pin.as_deref()).await?;
            tracing::info!("Synthetic load test session code: {code}");
            let key = crate::transport::crypto::SessionKey::derive(&code);

            let mut peer = PeerConnection::new(conn_state.clone(), ice_servers).await?;
            let offer_json = peer.create_offer().await?;
            server.upload_offer(&code, &offer_json, &key).await?;

            let answer_json = server.poll_answer(&code, &key, cancel_rx.clone()).await?;
            peer.set_answer(&answer_json).await?;
            server.delete_session(&code).await;

            let incoming_rx = peer.take_incoming_rx().expect("incoming_rx already taken");
            (peer.rtc_pc(), peer.data_channel(), incoming_rx)
        }
        Role::Answerer => {
            let code = args
                .pin
                .context("synthetic load test as --client needs --pin <session code>")?;
            let key = crate::transport::crypto::SessionKey::derive(&code);

            let ice_servers = server
                .join_session_polling(&code, cancel_rx.clone())
                .await?;
            let offer_json = server.poll_offer(&code, &key, cancel_rx.clone()).await?;

            let mut peer = PeerConnection::new(conn_state.clone(), ice_servers).await?;
            let answer_json = peer.accept_offer(&offer_json).await?;
            server.upload_answer(&code, &answer_json, &key).await?;

            let incoming_rx = peer.take_incoming_rx().expect("incoming_rx already taken");
            (peer.rtc_pc(), peer.data_channel(), incoming_rx)
        }
    };

    // The data channel is bidirectional and the peer is running this same
    // test, so it sends too. Nothing decodes what arrives, but it still has
    // to be drained or the unconsumed channel fills and blocks webrtc-rs's
    // message-delivery task, which would throttle our own send side too.
    tokio::spawn(async move { while incoming_rx.recv().await.is_some() {} });

    peer::spawn_sender(dc_holder, encoded_rx, conn_state.clone(), stats);

    tracing::info!("Waiting for the connection to come up...");
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
        while conn_state.get() != ConnState::Connected {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok();
    if !connected {
        anyhow::bail!("Timed out waiting for the connection to come up");
    }

    tracing::info!(
        "Connected. Sending {} KB frames at {} fps for {:.0}s; watch the 'pipeline | send' log \
         line for send avg, drop_buffered and max buffered.",
        args.frame_kb,
        args.fps,
        args.duration.as_secs_f64(),
    );

    let payload = vec![0xABu8; args.frame_kb as usize * 1024];
    let interval = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = Instant::now() + args.duration;

    while Instant::now() < deadline {
        ticker.tick().await;
        if encoded_tx.send(payload.clone()).await.is_err() {
            tracing::warn!("Sender task exited early");
            break;
        }
    }

    drop(encoded_tx);
    // Let the last second of diag stats flush before tearing down.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    if let Err(e) = pc.close().await {
        tracing::warn!("Failed to close peer connection: {e}");
    }
    Ok(())
}
