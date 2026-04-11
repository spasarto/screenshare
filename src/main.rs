mod app;
mod capture;
mod codec;
mod crypto;
mod transport;
mod turn;
mod viewer;

use anyhow::Result;

fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    // Validate required TURN environment variables early.
    for var in ["TURN_TOKEN_ID", "TURN_API_TOKEN"] {
        std::env::var(var)
            .map_err(|_| anyhow::anyhow!("{var} environment variable must be set (check .env)"))?;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Screen Share")
            .with_inner_size([960.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Screen Share",
        native_options,
        Box::new(|_cc| Ok(Box::new(viewer::App::new()))),
    )
    .map_err(|e| anyhow::anyhow!("Viewer error: {e}"))?;

    Ok(())
}
