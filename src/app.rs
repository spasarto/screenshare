use crate::capture;
use crate::codec::{H264Decoder, H264Encoder};
use crate::crypto::Crypto;
use crate::transport::peer::{self, PeerConnection};
use crate::viewer::{DecodedFrame, ViewerApp};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use windows_capture::window::Window;

const TARGET_WIDTH: u32 = 1280;
const TARGET_HEIGHT: u32 = 720;
const FRAME_CHANNEL_SIZE: usize = 4;

pub enum Role {
    Offerer,
    Answerer,
}

pub struct SessionConfig {
    pub passphrase: String,
    pub window: Window,
    pub role: Role,
}

pub fn run(config: SessionConfig) -> Result<()> {
    let crypto = Crypto::from_passphrase(&config.passphrase)?;

    // Frame buffer shared between decoder thread and egui viewer
    let frame_buffer: Arc<Mutex<Option<DecodedFrame>>> = Arc::new(Mutex::new(None));
    let connected = Arc::new(AtomicBool::new(false));

    // Capture → Encoder channel (std sync, both are OS threads)
    let (capture_tx, capture_rx) =
        std::sync::mpsc::sync_channel::<capture::Frame>(FRAME_CHANNEL_SIZE);

    // Encoder → Transport sender channel (tokio)
    let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_SIZE);

    // Build the tokio runtime for async WebRTC work
    let rt = tokio::runtime::Runtime::new()?;

    // Do signaling in the runtime
    let (_dc_holder, incoming_rx) = rt.block_on(async {
        let mut peer = PeerConnection::new().await?;

        match config.role {
            Role::Offerer => {
                let offer_blob = peer.create_offer(&crypto).await?;
                println!("\n========== YOUR CONNECTION CODE ==========");
                println!("{offer_blob}");
                println!("==========================================");
                println!("\nSend this code to your peer, then paste their response below:");

                let mut answer_input = String::new();
                std::io::stdin().read_line(&mut answer_input)?;
                peer.set_answer(answer_input.trim(), &crypto).await?;
            }
            Role::Answerer => {
                println!("\nPaste the connection code from your peer:");
                let mut offer_input = String::new();
                std::io::stdin().read_line(&mut offer_input)?;

                let answer_blob = peer.accept_offer(offer_input.trim(), &crypto).await?;
                println!("\n========== YOUR RESPONSE CODE ==========");
                println!("{answer_blob}");
                println!("=========================================");
                println!("\nSend this code back to your peer. Connecting...");
            }
        }

        let dc_holder = peer.data_channel();
        let incoming_rx = peer.take_incoming_rx().expect("incoming_rx already taken");

        // Spawn the network sender task
        peer::spawn_sender(dc_holder.clone(), encoded_rx);

        anyhow::Ok((dc_holder, incoming_rx))
    })?;

    tracing::info!("Signaling complete, starting capture and viewer");

    // Start the capture thread (Windows Graphics Capture, runs its own event loop)
    let _capture_handle =
        capture::wgc::start_capture(config.window, capture_tx, TARGET_WIDTH, TARGET_HEIGHT)?;

    // Start the encoder thread (CPU-bound, reads captured frames, sends H264)
    let _encoder_handle = std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            let mut encoder: Option<H264Encoder> = None;

            while let Ok(frame) = capture_rx.recv() {
                // Ensure dimensions are even (H264 requirement)
                let w = frame.width & !1;
                let h = frame.height & !1;

                // Lazily create encoder matching actual capture dimensions
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

                // Skip if frame data doesn't match expected size
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

    // Start the decoder thread (reads incoming H264 from transport, writes to frame buffer)
    let fb = frame_buffer.clone();
    let conn = connected.clone();
    let _decoder_handle = std::thread::Builder::new()
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
            conn.store(true, Ordering::Relaxed);

            while let Some(raw_msg) = incoming_rx.blocking_recv() {
                // Reassemble chunked messages
                let h264_data = match reassembler.feed(&raw_msg) {
                    Some(complete) => complete,
                    None => continue, // More chunks needed
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
                    Ok(None) => {} // Incomplete frame, need more data
                    Err(e) => {
                        tracing::warn!("Decode error: {e}");
                    }
                }
            }
            tracing::info!("Decoder thread exiting");
        })?;

    // Run the egui viewer on the main thread
    let viewer = ViewerApp::new(frame_buffer, connected);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Screen Share")
            .with_inner_size([960.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Screen Share",
        native_options,
        Box::new(|_cc| Ok(Box::new(viewer))),
    )
    .map_err(|e| anyhow::anyhow!("Viewer error: {e}"))?;

    Ok(())
}
