# screenshare

Peer-to-peer screen sharing for Windows. Captures a window, encodes it with H.264, and streams it over a WebRTC data channel to a remote viewer — no server relay required.

## Features

- **P2P via WebRTC** — Video is sent directly between peers using a WebRTC data channel with custom framing.
- **H.264 encoding** — Frames are encoded with OpenH264, at up to 720p / 30 fps by default (1080p, 1440p and 15 or 60 fps in settings).
- **Windows Graphics Capture** — Uses the native Windows Capture API for efficient, low-overhead window capture.
- **Cloudflare TURN** — The signaling server provisions short-lived TURN credentials from Cloudflare for NAT traversal.
- **Automatic reconnection** — ICE restarts with exponential backoff recover from network changes.
- **GUI** — Built with [egui](https://github.com/emilk/egui)/eframe. Pick a window, create or join a session, and view the stream.

## Prerequisites

- Windows 10/11
- [Rust](https://rustup.rs/) (stable)

## Building

```sh
cargo build --release
```

The binary is produced at `target/release/screenshare.exe`.

## Usage

### GUI (default)

```sh
screenshare
```

Launch the app, choose **Start a session** or **Join a session**, and pick a window (or a whole display) to share.

The host's waiting screen shows the session code, a share link and a QR of that link. The peer can paste the link straight into the code field — it's reduced to the code on the way in — or open it on a phone to read the code off the landing page.

### CLI flags

```
screenshare [OPTIONS]

Options:
      --server              Start in server mode (create session)
      --client              Start in client mode (join session)
      --process <PROCESS>   Process name to wait for and share (substring match, case-insensitive)
      --pin <PIN>           Session pin (6 digits + 4 lowercase letters)
```

**Examples:**

```sh
# Create a session sharing a window from "firefox", with a specific pin
screenshare --server --process firefox --pin 123456abcd

# Join an existing session and share a window from "notepad"
screenshare --client --process notepad --pin 123456abcd
```

When `--process` is specified, the app waits for a matching window to appear and selects it automatically.

### During a session

The toolbar shows the connection state and path (direct or relayed), and has **Fit** / **1:1** zoom, **Stats** (frame rate, bitrate and resolution each way, round-trip time, session length), **Fullscreen** (<kbd>F11</kbd>; <kbd>Esc</kbd> leaves it, and the bars hide once the pointer rests) and **Disconnect**.

The bar along the bottom shows what you're sending, with **Pause sharing** to stop sending without ending the session. The peer's screen says when you've paused.

Disconnecting, or closing the window, tells the peer straight away. If the link drops instead, the viewer shows the reconnect attempts as they happen. Once they run out, **Reconnect** starts the same session again: the host reopens the same code and the joiner looks it up, so both sides press Reconnect and nobody retypes anything.

### When something goes wrong

A failed session says what happened in plain words — offline, server unreachable, code not found, code already in use, session expired, peer never arrived, rate limited, capture refused — and offers the matching way forward (**Try again**, **Start with a new code**, **Check the code**, **Server settings**…). The raw error is under **Details**, with a copy button.

### Settings and the log

**Settings** (top-right of every screen outside a session) covers what you send (resolution and frame rate), theme, start minimised, and the signaling server address — both peers must use the same one.

**Log** (or <kbd>F10</kbd>, which also works mid-session) opens a drawer with the app's recent log output, filterable by level and copyable as plain text. It works in release builds, which have no console.

The app remembers its window size and position, your settings, whether you last started or joined, and what you last shared, in `%APPDATA%\Screen Share\data`.

## Architecture

```
┌──────────┐  H.264 frames   ┌────────────┐  WebRTC DC  ┌──────────┐
│  Capture  │───────────────▶│   Encoder   │────────────▶│   Peer   │
│  (WGC)   │                 │  (OpenH264) │             │ (WebRTC) │
└──────────┘                 └────────────┘             └────┬─────┘
                                                             │
                                                        Internet
                                                             │
┌──────────┐  RGBA frames     ┌────────────┐  WebRTC DC ┌────┴─────┐
│  Viewer  │◀────────────────│   Decoder   │◀───────────│   Peer   │
│  (egui)  │                 │  (OpenH264) │            │ (WebRTC) │
└──────────┘                 └────────────┘            └──────────┘
```

Both peers capture and encode, so each side sees the other's screen.

### Signaling server

A Cloudflare Worker (`worker/`) handles initial session setup:

1. **Create session** — generates a session code, provisions TURN credentials, and stores the (encrypted) SDP offer/answer in a Durable Object. A per-IP rate limit on the code-gated endpoints makes brute-forcing a code infeasible.
2. **Join session** — the other peer polls for the offer, uploads its answer.
3. After the WebRTC connection is established, the host deletes the session from the server. All further signaling happens in-band over the data channel: ICE restarts, plus session control messages (hang-up, pause/resume, round-trip pings; see `src/transport/control.rs`).

It also serves `GET /j/:code`, the landing page behind the share link and QR. That page is static — it neither creates nor looks up the session, so opening a link costs nothing from the rate-limit budget and reveals nothing about whether the code exists.

### Key modules

| Module | Description |
|---|---|
| `src/main.rs` | CLI parsing, app launch |
| `src/app.rs` | Session orchestration (offerer/answerer flows) |
| `src/failure.rs` | Classifies session errors into user-facing failures and recoveries |
| `src/logging.rs` | In-memory log buffer behind the log drawer |
| `src/capture/` | Window enumeration and WGC-based frame capture |
| `src/codec/` | H.264 encode/decode via OpenH264 |
| `src/transport/` | WebRTC peer connection, data channel framing, signaling |
| `src/viewer/` | egui-based GUI |
| `worker/` | Cloudflare Worker signaling server |

## Logging

The in-app log drawer (**Log**, or <kbd>F10</kbd>) records this app's debug output and everything else at info and up. Set `RUST_LOG` to change what's recorded, both there and on the console of a debug build:

```sh
set RUST_LOG=info
screenshare
```

## License

See [Cargo.toml](Cargo.toml) for package metadata.
