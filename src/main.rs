mod app;
mod capture;
mod codec;
mod crypto;
mod transport;
mod viewer;

use anyhow::Result;
use std::io::Write;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    println!("=== P2P Screen Share ===\n");

    // 1. Get passphrase
    let passphrase = rpassword::prompt_password("Enter passphrase: ")?;
    anyhow::ensure!(!passphrase.is_empty(), "Passphrase cannot be empty");

    // 2. Pick window to share
    let window = capture::picker::pick_window_interactive()?;

    // 3. Choose role
    print!("\nCreate session (c) or Join session (j)? ");
    std::io::stdout().flush()?;
    let mut role_input = String::new();
    std::io::stdin().read_line(&mut role_input)?;

    let role = match role_input.trim().to_lowercase().as_str() {
        "c" | "create" => app::Role::Offerer,
        "j" | "join" => app::Role::Answerer,
        other => anyhow::bail!("Unknown role: '{other}'. Use 'c' or 'j'."),
    };

    // 4. Run the app
    app::run(app::SessionConfig {
        passphrase,
        window,
        role,
    })
}
