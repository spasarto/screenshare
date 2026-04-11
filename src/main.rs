mod app;
mod capture;
mod codec;
mod signaling_server;
mod transport;
mod viewer;

use anyhow::Result;
use clap::Parser;

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

    /// 6-digit session pin. Server: use this pin instead of generating one. Client: connect to this pin.
    #[arg(long)]
    pub pin: Option<String>,
}

fn main() -> Result<()> {
    let args = CliArgs::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Validate pin format if provided
    if let Some(ref pin) = args.pin {
        if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
            anyhow::bail!("Pin must be exactly 6 digits (e.g. 123456)");
        }
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Screen Share")
            .with_inner_size([960.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Screen Share",
        native_options,
        Box::new(move |_cc| Ok(Box::new(viewer::App::new(args)))),
    )
    .map_err(|e| anyhow::anyhow!("Viewer error: {e}"))?;

    Ok(())
}
