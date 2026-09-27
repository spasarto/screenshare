#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod capture;
mod codec;
mod diag;
mod failure;
mod logging;
mod signaling_server;
mod synthetic_load;
mod transport;
mod viewer;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

/// What the in-app log drawer records unless `RUST_LOG` says otherwise:
/// this app's own debug output, everyone else's info and up.
const DRAWER_LOG_FILTER: &str = "info,screenshare=debug";

#[derive(Parser, Debug, Clone)]
#[command(name = "screenshare", about = "Peer-to-peer screen sharing")]
pub struct CliArgs {
    /// Start in server mode (create session)
    #[arg(long, conflicts_with = "client")]
    pub server: bool,

    /// Start in client mode (join session)
    #[arg(long, conflicts_with = "server")]
    pub client: bool,

    /// Process name to wait for and share (substring match, case-insensitive)
    #[arg(long)]
    pub process: Option<String>,

    /// Session pin (6 digits + 4 lowercase letters). Server: use this pin instead of
    /// generating one. Client: connect to this pin.
    #[arg(long)]
    pub pin: Option<String>,

    /// Debug: skip capture and the encoder, sending fixed-size synthetic
    /// frames at a fixed rate instead, to measure what the data channel can
    /// sustain independent of encode cost (docs/hardware-encoding-plan.md
    /// phase 0). Requires --server or --client and --synthetic-fps.
    #[arg(long, requires = "synthetic_fps")]
    pub synthetic_kb: Option<u32>,

    /// Frame rate for --synthetic-kb.
    #[arg(long, requires = "synthetic_kb")]
    pub synthetic_fps: Option<u32>,

    /// How long to run the --synthetic-kb throughput check, in seconds.
    #[arg(long, default_value_t = 30, requires = "synthetic_kb")]
    pub synthetic_secs: u64,
}

/// A valid session pin is 6 ASCII digits followed by 4 lowercase ASCII letters,
/// matching the format the signaling server generates and accepts.
fn is_valid_pin(pin: &str) -> bool {
    let bytes = pin.as_bytes();
    bytes.len() == 10
        && bytes[..6].iter().all(|b| b.is_ascii_digit())
        && bytes[6..].iter().all(|b| b.is_ascii_lowercase())
}

fn main() -> Result<()> {
    let args = CliArgs::parse();

    // Must happen before any TLS or DTLS: with two providers compiled in,
    // rustls panics on first use rather than guess (see Cargo.toml).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Two sinks: the console (debug builds have one; release builds are a
    // windowed app and don't), and the buffer behind the in-app log drawer.
    let log_buffer = logging::LogBuffer::new();
    let env_filter = |default: &str| {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter("info")))
        .with(logging::layer(log_buffer.clone()).with_filter(env_filter(DRAWER_LOG_FILTER)))
        .init();

    // Validate pin format if provided
    if let Some(ref pin) = args.pin {
        if !is_valid_pin(pin) {
            anyhow::bail!("Pin must be 6 digits followed by 4 lowercase letters (e.g. 482917bcdq)");
        }
    }

    // --client --process auto-starts the join as soon as the target window
    // appears, with no setup screen in between to type a code into — so
    // without --pin there's no way the session code ever reaches the app.
    if args.client && args.process.is_some() && args.pin.is_none() {
        anyhow::bail!(
            "--client --process requires --pin (there's no setup screen to enter it once auto-start begins)"
        );
    }

    if let Some(frame_kb) = args.synthetic_kb {
        if !args.server && !args.client {
            anyhow::bail!("--synthetic-kb requires --server or --client");
        }
        let fps = args.synthetic_fps.expect("clap enforces synthetic_fps with synthetic_kb");
        return synthetic_load::run(synthetic_load::SyntheticLoadArgs {
            server_url: viewer::settings::DEFAULT_SERVER_URL.to_string(),
            role: if args.server { app::Role::Offerer } else { app::Role::Answerer },
            pin: args.pin,
            frame_kb,
            fps,
            duration: std::time::Duration::from_secs(args.synthetic_secs),
        });
    }

    let icon = {
        let bytes = include_bytes!("logo.png");
        let img = image::load_from_memory(bytes)
            .expect("Failed to load logo.png")
            .to_rgba8();
        let (width, height) = img.dimensions();
        egui::viewport::IconData {
            rgba: img.into_raw(),
            width,
            height,
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Screen Share")
            .with_inner_size([960.0, 540.0])
            // Below this the picker grid and the viewer toolbar stop fitting.
            .with_min_inner_size([720.0, 540.0])
            .with_icon(icon),
        // Size and position are restored from the last run (eframe's own
        // window persistence); the app's own state is saved in `App::save`.
        persist_window: true,
        ..Default::default()
    };

    eframe::run_native(
        "Screen Share",
        native_options,
        Box::new(move |cc| Ok(Box::new(viewer::App::new(args, log_buffer, cc)))),
    )
    .map_err(|e| anyhow::anyhow!("Viewer error: {e}"))?;

    Ok(())
}
