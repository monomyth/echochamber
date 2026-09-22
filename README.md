# echochamber

<img src="assets/standby.jpg" alt="A dark control room with a red chair and a warm lamp. This is the waiting video." width="880">

You stream from OBS to **one** place: this program, running on your computer. It sends that same live video to YouTube, X, and Twitch.

If OBS stops, viewers do not get a dead stream. They see the waiting video above, with its music, until you start OBS again. Then your show comes straight back.

The waiting video is included. [Play it](assets/standby.mp4).

You do not need a YouTube, X, or Twitch account to try the program. You only need those keys when you want the stream to actually appear on a site.

## What you need

- [Rust](https://rustup.rs), so the program can be built
- [ffmpeg](https://ffmpeg.org), so the video can be sent onward
- OBS, or any other program that can send RTMP

RTMP is just the usual way OBS sends a live stream to a server.

## Start it

In this folder:

```bash
cargo build --release
./target/release/echochamber init
./target/release/echochamber serve
```

`init` creates `config.toml`. That file is where settings and stream keys go. It is ignored by git, so keys are not committed by accident. `init` will not overwrite a `config.toml` that is already there.

`serve` starts the program and leaves it running. Leave that terminal open.

## Point OBS at it

| OBS field | Value |
|---|---|
| Server | `rtmp://127.0.0.1:1935/echochamber` |
| Stream key | `live` |

`127.0.0.1` means "this computer." Port `1935` is the standard RTMP port.

Put `live` in the stream key box. Do not stick `/live` on the end of the server address. If you do, it still works. The cleaner split is the one in the table.

Press **Start Streaming** in OBS. Nothing is on YouTube yet. The video is only arriving on your own machine. That is enough to confirm the program is receiving it:

```bash
./target/release/echochamber status
```

Look for `"publishing": true`.

To test without OBS, send a test picture:

```bash
ffmpeg -re -f lavfi -i testsrc=size=1280x720:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
  -f flv rtmp://127.0.0.1:1935/echochamber/live
```

## Send it to a website

Open `config.toml`. Each site is off until you set `enabled = true` and paste that site's stream key.

```toml
[platforms.youtube]
enabled = true
stream_key = "paste the YouTube key here"

[platforms.x]
enabled = true
stream_key = "paste the X key here"

[platforms.twitch]
enabled = true
stream_key = "paste the Twitch key here"
```

Where to copy the key from:

- **YouTube:** [YouTube Studio](https://studio.youtube.com) → Go live → Stream key
- **X:** [studio.x.com/producer](https://studio.x.com/producer) → create an RTMP source → copy the key. X has one more step: after this program connects, open Broadcasts, create a broadcast, and go live. Until you do that, X accepts the video but does not show it.
- **Twitch:** [Dashboard → Settings → Stream](https://www.twitch.tv/dashboard/settings/stream) → Primary Stream Key

Save the file, then tell the running program to read it again. OBS can keep streaming.

```bash
./target/release/echochamber validate
./target/release/echochamber reload
```

`validate` checks the file before you reload it. It fails if a site is turned on with no stream key, if captions are on and the Whisper model file is missing, or if the waiting video is on and its file cannot be found. `status` should then show each site you enabled as `"state": "running"`.

By default the video is **copied**, not recompressed. If OBS is sending 1080p, viewers get 1080p. If OBS is sending a larger picture, that larger picture is what goes out. To make this program recompress instead, set this in `config.toml`:

```toml
[quality]
mode = "reencode"   # copy is the default and uses much less CPU
```

`reload` only rereads the config. If you build a new version of the program, stop it and start it again:

```bash
./target/release/echochamber stop
./target/release/echochamber serve
```

To add one extra destination without editing the file:

```bash
./target/release/echochamber push --url rtmp://example.com/app/stream-key
```

That extra destination lasts until you stop the program.

## The waiting video

It is off until you turn it on.

```toml
[standby]
enabled = true
after_first_publish = true
video = "assets/standby.mp4"
audio = "assets/standby.m4a"
width = 1920
height = 1080
fps = 30
delay_secs = 2
```

| Setting | What it does |
|---|---|
| `enabled` | `true` shows the waiting video while OBS is not streaming. |
| `after_first_publish` | `true` waits until you have gone live once. The video then covers a dropout, not the time before the show starts. `false` also shows it before the first stream. |
| `video` | The looping picture. The one in this repo is fine. Use another file if you want. |
| `audio` | The looping music. Leave it empty for silence. |
| `width`, `height`, `fps` | Size of the **waiting** video only. Your live OBS stream is not resized. |
| `delay_secs` | How many seconds to wait after OBS stops. A two-second hiccup then does not flash the waiting video. |

All three sites share that one waiting video. It is not encoded three times.

Change the file or the size, run `reload`, and the next dropout uses the new one. `status` shows `"standby": true` while it is on screen.

## From another computer on your network

By default only programs on this computer can connect. To let OBS on another machine on the same network connect:

```bash
./target/release/echochamber serve --host 0.0.0.0
```

In OBS on that other machine, replace `127.0.0.1` with this computer's address, for example `rtmp://192.168.1.20:1935/echochamber`. The stream key is still `live`.

## Captions, if you want them

Off by default. This uses [whisper.cpp](https://github.com/ggml-org/whisper.cpp) to turn speech into subtitles while you are live.

```toml
[subtitles]
enabled = true
language = "auto"          # auto, en, or ru
whisper_model = "/path/to/ggml-small.bin"
sidecar = true             # write subtitle files next to the program
burn_in = false            # true stamps words onto the video itself
style = "modern"           # modern, minimal, or broadcast
```

`sidecar` writes `echochamber_live.srt` and `echochamber_live.ass`. Those are separate subtitle files. `burn_in` paints the words into the video, which needs a copy of ffmpeg built with libass. The ffmpeg from Homebrew usually cannot do that. Leave `burn_in` false unless you have one that can, and point `ECHOCHAMBER_FFMPEG_BIN` at it.

## Commands

Run these from the same folder, in another terminal, while `serve` is running.

| Command | What it does |
|---|---|
| `init` | Create `config.toml`. |
| `serve` | Run the program. |
| `status` | Show what is happening, as JSON. |
| `validate` | Check `config.toml` and that ffmpeg is installed. Add `--verbose` to see the ffmpeg command, with stream keys hidden. |
| `reload` | Read `config.toml` again. |
| `stop` | Quit the program. |
| `push --url ...` | Send the stream to one more address, until you quit. |

The program looks for its config in this order:

1. `--config path/to/file.toml`
2. the `ECHOCHAMBER_CONFIG` environment variable
3. `./config.toml`
4. `~/.config/echochamber/config.toml`

`status` looks like this. `bps` is bits per second, a measure of how much video is moving. The stream key is shown with most characters hidden.

```json
{
  "ingest": { "publishing": true, "standby": false },
  "pushers": [
    { "name": "youtube", "state": "running", "bps": 8200000 }
  ],
  "bandwidth": { "ingest_bps": 8000000, "push_bps": 8200000 }
}
```

`publishing` means OBS is connected. `standby` means viewers are seeing the waiting video. `ingest_bps` is video coming in from OBS. `push_bps` is video going out to the sites.

The same actions exist as HTTP on port 8080, on this computer only, unless you changed the bind address. You can ignore these if the commands above are enough.

| Request | Same as |
|---|---|
| `GET /status` | `status` |
| `POST /reload` | `reload` |
| `POST /stop` | `stop` |
| `POST /push` with `{"url": "..."}` | `push --url` |

## License

[MIT](LICENSE). The waiting video and its music are part of this repo. You can use them with it.
