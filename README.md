# echochamber

Local RTMP relay: OBS (or ffmpeg) publishes once, echochamber fans the stream out to YouTube and X, optionally running live Whisper subtitles.

This is a rebuild of the May 2026 worktree (session `019e4126`). The original source was never committed and the worktree is gone; behaviour is reconstructed from session memory.

## Quick start

```bash
cd echochamber
cargo build --release
./target/release/echochamber init
# edit config.toml — at least ingest.stream_key, plus platform keys if you want to go live
./target/release/echochamber validate --verbose
RUST_LOG=echochamber=debug ./target/release/echochamber serve
```

OBS (preferred):

- **Server:** `rtmp://127.0.0.1:1935/echochamber`  *(app only — do not append `/live`)*
- **Stream key:** `live` (or `ingest.stream_key`)

If the server URL is `rtmp://127.0.0.1:1935/echochamber/live`, OBS publishes as app `echochamber/live`. echochamber will still accept that and play it back, but the preferred split above matches the config.

Relay-only test (no platforms, no real broadcast):

```bash
ffmpeg -re -f lavfi -i testsrc=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=44100 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
  -f flv rtmp://127.0.0.1:1935/echochamber/live
```

## CLI

| Command | Role |
|---|---|
| `init` | Write a commented `config.toml` |
| `validate [--verbose]` | Masked URLs, ffmpeg/whisper probe, command templates |
| `serve` | Daemon: RTMP ingest + HTTP control + pushers + ASR. `--host 0.0.0.0` listens on all interfaces (ports from config). `--ingest-bind` / `--control-bind` override a full `host:port`. |
| `status` / `reload` / `stop` | Talk to the control plane |
| `push --url rtmp://...` | Temporary extra destination (no config edit) |

Config search order: `--config`, `$ECHOCHAMBER_CONFIG`, `./config.toml`, `~/.config/echochamber/config.toml`.

## Binaries

| Env | Default | Notes |
|---|---|---|
| `ECHOCHAMBER_FFMPEG_BIN` | `ffmpeg` | Burn-in needs `ass`/`subtitles` (libass). Homebrew 9.x often has neither. |
| `ECHOCHAMBER_WHISPER_BIN` | `whisper-cli` | whisper.cpp CLI |

A libass ffmpeg without replacing the system one:

```bash
brew tap homebrew-ffmpeg/ffmpeg
brew install homebrew-ffmpeg/ffmpeg/ffmpeg
export ECHOCHAMBER_FFMPEG_BIN="$(brew --prefix homebrew-ffmpeg/ffmpeg/ffmpeg)/bin/ffmpeg"
```

## YouTube, X, and Twitch

Keep OBS publishing to localhost. echochamber copies that stream out.

1. Get keys
   - **YouTube:** [YouTube Studio](https://studio.youtube.com) → Create → Go live → copy **Stream key**. Optional RTMPS base: `rtmps://a.rtmps.youtube.com/live2`.
   - **X:** [studio.x.com/producer](https://studio.x.com/producer) → Sources → Create Source → copy **RTMP(s) stream key**. After the encoder is connected, **Broadcasts → Create Broadcast** and go live (otherwise X ingest accepts the stream but nothing appears on the timeline).
   - **Twitch:** [Creator Dashboard → Settings → Stream](https://www.twitch.tv/dashboard/settings/stream) → **Primary Stream Key**. Optional RTMPS: `rtmps://live.twitch.tv:443/app`.
2. In `config.toml`:

```toml
[platforms.youtube]
enabled = true
stream_key = "YOUR_YOUTUBE_KEY"
# ingest_base = "rtmps://a.rtmps.youtube.com/live2"

[platforms.x]
enabled = true
stream_key = "YOUR_X_KEY"
# ingest_base = "rtmp://va.pscp.tv:80/x"

[platforms.twitch]
enabled = true
stream_key = "YOUR_TWITCH_KEY"
# ingest_base = "rtmp://live.twitch.tv/app"
```

3. Apply without stopping OBS:

```bash
./target/release/echochamber validate
./target/release/echochamber reload
./target/release/echochamber status
```

`pushers` should show `youtube` / `x` / `twitch` as `running`. One-off extra destination: `echochamber push --url rtmp://host/app/key`.

`quality.mode = "copy"` (default) uses whatever bitrate OBS is already sending, once per enabled platform.

OBS stop then start again is a new publish session. echochamber waits for the previous ffmpeg processes to release the destination sockets and for a new keyframe before it republishes, so YouTube / X / Twitch are not hit with two overlapping RTMP connections or leftover GOP timestamps. `reload` only rereads config — binary changes need a `stop` then `serve`.

## Standby slate

Off unless `[standby] enabled = true`. Then dests loop a **custom video** while waiting for OBS. Default `after_first_publish = true` means the slate only starts after OBS has been live once (a drop / resume), not before the first go-live.

```toml
[standby]
enabled = true
after_first_publish = true
video = "assets/standby.mp4"   # any local mp4/mov
audio = "assets/standby.m4a"   # empty = silence
width = 1920
height = 1080
fps = 30
delay_secs = 2
```

Encode is on the fly to `width` × `height` @ `fps`. `reload` picks up path/size changes. `status` JSON: `ingest.standby`.

## Bandwidth

Rates are bits/sec over a ~1s window (independent of how often you poll). Totals are cumulative bytes.

- **IN** — FLV media payload from OBS (`on_media_tag` while publishing)
- **OUT** — ffmpeg muxed bytes per destination (`total_size` from `-progress`, sampled every 0.2s)

```bash
./target/release/echochamber status
./target/release/echochamber status | jq .bandwidth
```

JSON fields: `bandwidth.ingest_bps`, `ingest_bps_avg`, `push_bps`, `push_bps_avg`, plus per-pusher `bps` / `bytes_out`.

## Architecture

OBS → local `rtmp-rs` server (GOP cache) → per-destination ffmpeg (`-c copy` or re-encode) → YouTube / X / Twitch.

ASR: ffmpeg pulls 8s mono WAV from the local RTMP URL, `whisper-cli` transcribes, cues go to `echochamber_live.srt` / `echochamber_live.ass`. Burn-in uses `ass=filename=echochamber_live.ass` (no `force_style` quoting).

Control HTTP (default `127.0.0.1:8080`): `GET /status`, `POST /reload`, `POST /stop`, `POST /push`.
