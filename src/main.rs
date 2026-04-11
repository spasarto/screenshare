mod app;
mod capture;
mod codec;
mod crypto;
mod transport;
mod viewer;

use anyhow::Result;

fn main() -> Result<()> {
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
