# webrtc crate upgrade investigation

Date: 2026-09-14
Status: **Not started.** Investigated and deferred — see recommendation at the end.

## Current state

- `Cargo.toml` pins `webrtc = "0.17"`, resolved in `Cargo.lock` to **0.17.2**.
- Used in three files:
  - `src/transport/peer.rs` (643 lines) — the bulk of the usage: connection setup, event
    callbacks, data channel send/receive, ICE restart, stats.
  - `src/transport/signaling.rs` — SDP encode/decode for the signaling blob.
  - `src/signaling_server.rs` — `RTCIceServer` construction from server config.
- `src/transport/framing.rs` (the chunking/reassembly protocol from CLN-05) has **no**
  dependency on the `webrtc` crate and is unaffected by any future upgrade.

## Latest available version

Latest stable on crates.io: **0.20.5** (0.21 is in release-candidate at time of writing).
Release history jumps straight from `v0.17.2` to `v0.20.0-alpha.1` — there was no
0.18/0.19 in between.

## This is not a normal version bump

`0.20.0`'s own changelog states it is *"a clean, ergonomic, runtime-agnostic rewrite on
top of a Sans-I/O core."* This was verified directly against the 0.20 source and
examples (not just the changelog blurb) and is a ground-up API redesign, not an
incremental change.

| Area | 0.17 (current) | 0.20 (latest) |
|---|---|---|
| Connection setup | `APIBuilder` + `MediaEngine` + `Registry` → `api.new_peer_connection()` | `PeerConnectionBuilder` fluent builder (`.with_runtime(...)`, `.with_handler(...)`) |
| Events | Separate closures per event: `on_peer_connection_state_change`, `on_data_channel`, `on_ice_gathering_state_change`, each `Box::new(move \|..\| Box::pin(async move {..}))` | One `PeerConnectionEventHandler` trait (`#[async_trait]`) with one method per event |
| Data channel messages | `dc.on_message(Box::new(...))` callback | `dc.poll().await -> Option<DataChannelEvent>` (`OnOpen`/`OnMessage`/`OnClose`), consumed in a loop |
| `DataChannel` type | `Arc<RTCDataChannel>` concrete struct | `Arc<dyn DataChannel>` trait object; `label()` is now `async fn -> Result<String>`; `send()` takes `BytesMut`, not `Bytes` |
| Send backpressure | Hand-rolled: `MAX_BUFFERED_AMOUNT` check + drop-frame logic in `spawn_sender` (added in `e2d0766`) | Built in: `PeerConnectionBuilder::with_data_channel_send_buffer_limit()` + `DataChannel::try_send()` (fails fast, mirrors `mpsc::try_send`) |
| Stats | `pc.get_stats().await -> RTCStatsReport` (no args); match on `webrtc::stats::StatsReportType` | `pc.get_stats(now: Instant, selector: StatsSelector).await`; types now live under `rtc::statistics` |
| Runtime | Implicit — crate calls `tokio::spawn` internally | Explicit `Runtime` trait injected via `.with_runtime(Arc::new(TokioRuntime))`; `runtime-tokio` is still the default feature, so plain tokio use keeps working, but the injection point is new |
| SDP types | `webrtc::peer_connection::sdp::session_description::RTCSessionDescription` | Re-exported at `webrtc::peer_connection::RTCSessionDescription` (import path changed; the offer/answer/pranswer construction in `signaling.rs` needs re-verification against the new type) |

Sources checked: `webrtc-rs/webrtc` CHANGELOG.md, GitHub releases, crate source at tag
`v0.20.0` (`src/peer_connection/mod.rs`, `src/data_channel/mod.rs`), the crate's own
`Cargo.toml`, and the `data-channels`, `ice-restart`, `data-channels-flow-control`, and
`stats` examples at that tag.

## Scope of impact on this repo

- **`peer.rs` needs a substantial rewrite.** Every registered callback becomes a method
  on a `PeerConnectionEventHandler` impl; the `on_message` push model becomes a `poll()`
  pull loop. The ICE-restart offerer/answerer flow and the reconnect-monitor state
  machine both need to be re-verified against the new event shape, not just recompiled.
- **`signaling.rs` / `signaling_server.rs`** need smaller import-path fixes
  (`RTCSessionDescription`, `RTCIceServer`, `RTCSdpType` all moved), plus verification
  that SDP construction (`RTCSessionDescription::offer(..)`/`::answer(..)`, and the
  pranswer/rollback serde-construction workaround) still behaves the same.
- **`framing.rs`** (CLN-05 chunking/reassembly, with its unit tests) is untouched.

## Notable overlap with recent work

The reconnect/backpressure hardening just landed in `e2d0766` (`BUG-04, BUG-05, BUG-08,
BUG-10`) hand-rolls exactly what 0.20 now provides natively: a per-channel send buffer
limit with fail-fast behavior. Upgrading later isn't purely added cost — it would let us
delete the custom `MAX_BUFFERED_AMOUNT` / drop-on-overflow logic in `spawn_sender` in
favor of `DataChannel::try_send()` plus
`PeerConnectionBuilder::with_data_channel_send_buffer_limit()`.

## Compatibility shim

None exists. Checked the 0.20 `Cargo.toml`/source and the `webrtc-rs` GitHub org's
repositories directly — there is no legacy/compat crate or feature flag to ease the
transition. It is a hard cutover from the 0.17 API shape.

## Toolchain

0.20 requires **edition 2024**. Installed toolchain is `rustc 1.98.1` / `cargo 1.98.1`,
well past what edition 2024 requires — not a blocker.

## Recommendation

Treat this as a dedicated migration project (realistically a day-plus, given the
reconnect/ICE-restart logic needs careful behavioral re-verification against the new
poll-based event model), not something to fold into an unrelated change. Decision made
2026-09-14: **defer** — stay on `webrtc = "0.17"` for now and revisit this document when
ready to take it on.
