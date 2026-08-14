# screenshare

Peer-to-peer screen sharing for Windows. Captures a window, encodes it with H.264, and streams it over a WebRTC data channel to a remote viewer — no server relay required.

## Features

- **P2P via WebRTC** — Video is sent directly between peers using a WebRTC data channel with custom framing.
- **H.264 encoding** — Frames are encoded with OpenH264 at 1280×720 for low-bandwidth streaming.
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

Launch the app, choose **Create** or **Join**, select a window to share, and exchange the session code with the remote peer.

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
3. After the WebRTC connection is established, all further signaling (ICE restarts) happens in-band over the data channel.

### Key modules

| Module | Description |
|---|---|
| `src/main.rs` | CLI parsing, app launch |
| `src/app.rs` | Session orchestration (offerer/answerer flows) |
| `src/capture/` | Window enumeration and WGC-based frame capture |
| `src/codec/` | H.264 encode/decode via OpenH264 |
| `src/transport/` | WebRTC peer connection, data channel framing, signaling |
| `src/viewer/` | egui-based GUI |
| `worker/` | Cloudflare Worker signaling server |

## Logging

Set the `RUST_LOG` environment variable to control log output:

```sh
set RUST_LOG=info
screenshare
```

## License

See [Cargo.toml](Cargo.toml) for package metadata.
