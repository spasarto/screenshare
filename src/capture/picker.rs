use anyhow::Result;
use windows_capture::window::Window;

pub struct WindowInfo {
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
        result.push(WindowInfo { title, window: w });
    }
    Ok(result)
}

/// Find the first window whose process name contains the given substring (case-insensitive).
pub fn find_window_by_process(name: &str) -> Option<WindowInfo> {
    let windows = enumerate_windows().ok()?;
    let needle = name.to_lowercase();
    windows.into_iter().find(|w| match w.window.process_name() {
        Ok(proc_name) => proc_name.to_lowercase().contains(&needle),
        Err(_) => false,
    })
}
