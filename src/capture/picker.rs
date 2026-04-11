use anyhow::Result;
use windows_capture::window::Window;

pub struct WindowInfo {
    pub index: usize,
    pub title: String,
    pub window: Window,
}

pub fn enumerate_windows() -> Result<Vec<WindowInfo>> {
    let windows =
        Window::enumerate().map_err(|e| anyhow::anyhow!("Failed to enumerate windows: {e}"))?;

    let mut result = Vec::new();
    for w in windows {
        let title = match w.title() {
            Ok(t) if !t.is_empty() => t,
            _ => continue,
        };
        result.push(WindowInfo {
            index: result.len(),
            title,
            window: w,
        });
    }
    Ok(result)
}

pub fn pick_window_interactive() -> Result<Window> {
    let windows = enumerate_windows()?;
    anyhow::ensure!(!windows.is_empty(), "No visible windows found");

    println!("\nAvailable windows:");
    println!("{:-<60}", "");
    for w in &windows {
        println!("  [{:>3}] {}", w.index, w.title);
    }
    println!("{:-<60}", "");
    print!("Select window number: ");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let idx: usize = input
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid number"))?;

    let selected = windows
        .into_iter()
        .find(|w| w.index == idx)
        .ok_or_else(|| anyhow::anyhow!("Window {idx} not found"))?;

    println!("Selected: {}", selected.title);
    Ok(selected.window)
}
