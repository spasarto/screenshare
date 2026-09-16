use anyhow::Result;
use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

pub struct WindowInfo {
    pub title: String,
    pub window: Window,
}

pub struct MonitorInfo {
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub monitor: Monitor,
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

/// One tile per active display, labelled "Entire screen" when there's only
/// one so the common case doesn't force the user to think about which
/// monitor they mean.
pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    let Ok(monitors) = Monitor::enumerate() else {
        return Vec::new();
    };
    let multiple = monitors.len() > 1;
    monitors
        .into_iter()
        .enumerate()
        .map(|(i, monitor)| {
            let label = if multiple {
                format!("Monitor {}", i + 1)
            } else {
                "Entire screen".to_string()
            };
            MonitorInfo {
                label,
                width: monitor.width().unwrap_or(0),
                height: monitor.height().unwrap_or(0),
                monitor,
            }
        })
        .collect()
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
