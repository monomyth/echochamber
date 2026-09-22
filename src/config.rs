use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::redact::redact_key;

pub const DEFAULT_YOUTUBE_INGEST: &str = "rtmp://a.rtmp.youtube.com/live2";
pub const DEFAULT_X_INGEST: &str = "rtmp://va.pscp.tv:80/x";
pub const DEFAULT_TWITCH_INGEST: &str = "rtmp://live.twitch.tv/app";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ingest: IngestConfig,
    #[serde(default)]
    pub control: ControlConfig,
    #[serde(default)]
    pub platforms: Platforms,
    #[serde(default)]
    pub subtitles: SubtitlesConfig,
    #[serde(default)]
    pub quality: QualityConfig,
    #[serde(default)]
    pub standby: StandbyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestConfig {
    /// Bind address for the local RTMP server (OBS publishes here).
    #[serde(default = "default_ingest_bind")]
    pub bind: String,
    /// RTMP application name. OBS server URL: rtmp://HOST:PORT/<app>
    #[serde(default = "default_app")]
    pub app: String,
    /// Stream key OBS (and ffmpeg play) must use.
    #[serde(default = "default_stream_key")]
    pub stream_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    #[serde(default = "default_control_bind")]
    pub bind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Platforms {
    pub youtube: Option<YoutubePlatform>,
    pub x: Option<XPlatform>,
    pub twitch: Option<TwitchPlatform>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoutubePlatform {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub stream_key: String,
    /// Override the YouTube RTMP ingest base. Default: rtmp://a.rtmp.youtube.com/live2
    pub ingest_base: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XPlatform {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub stream_key: String,
    /// Override X/Periscope ingest base. Default: rtmp://va.pscp.tv:80/x
    pub ingest_base: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitchPlatform {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub stream_key: String,
    /// Override Twitch ingest base. Default: rtmp://live.twitch.tv/app
    /// RTMPS: rtmps://live.twitch.tv:443/app
    pub ingest_base: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitlesConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `auto`, `en`, or `ru`
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub burn_in: bool,
    #[serde(default = "default_true")]
    pub sidecar: bool,
    #[serde(default)]
    pub style: SubtitleStyle,
    /// Path to a whisper.cpp ggml/gguf model.
    #[serde(default)]
    pub whisper_model: String,
    /// Seconds of audio pulled per Whisper invocation.
    #[serde(default = "default_chunk_seconds")]
    pub chunk_seconds: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleStyle {
    #[default]
    Modern,
    Minimal,
    Broadcast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandbyConfig {
    /// Off unless set true. When on, dests loop `video` while OBS is down.
    #[serde(default)]
    pub enabled: bool,
    /// Only after OBS has published once (waiting for resume). If false, slate
    /// also runs before the first go-live.
    #[serde(default = "default_true")]
    pub after_first_publish: bool,
    /// Custom loop clip. Relative paths: cwd, config dir, then binary dir.
    #[serde(default = "default_standby_video")]
    pub video: String,
    /// Optional looping audio bed (m4a/mp3/wav). Empty = silence.
    #[serde(default = "default_standby_audio")]
    pub audio: String,
    #[serde(default = "default_standby_width")]
    pub width: u32,
    #[serde(default = "default_standby_height")]
    pub height: u32,
    #[serde(default = "default_standby_fps")]
    pub fps: u32,
    /// Wait this long after OBS drops before starting the slate (skip on a hitch).
    #[serde(default = "default_standby_delay")]
    pub delay_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityConfig {
    #[serde(default)]
    pub mode: QualityMode,
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default = "default_crf")]
    pub crf: u8,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QualityMode {
    #[default]
    Copy,
    Reencode,
}

fn default_ingest_bind() -> String {
    "127.0.0.1:1935".into()
}
fn default_app() -> String {
    "echochamber".into()
}
fn default_stream_key() -> String {
    "live".into()
}
fn default_control_bind() -> String {
    "127.0.0.1:8080".into()
}
fn default_language() -> String {
    "auto".into()
}
fn default_true() -> bool {
    true
}
fn default_chunk_seconds() -> u64 {
    8
}
fn default_preset() -> String {
    "veryfast".into()
}
fn default_crf() -> u8 {
    23
}
fn default_standby_video() -> String {
    "assets/standby.mp4".into()
}
fn default_standby_audio() -> String {
    "assets/standby.m4a".into()
}
fn default_standby_width() -> u32 {
    1920
}
fn default_standby_height() -> u32 {
    1080
}
fn default_standby_fps() -> u32 {
    30
}
fn default_standby_delay() -> u64 {
    2
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            bind: default_ingest_bind(),
            app: default_app(),
            stream_key: default_stream_key(),
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            bind: default_control_bind(),
        }
    }
}

impl Default for SubtitlesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            language: default_language(),
            burn_in: false,
            sidecar: true,
            style: SubtitleStyle::Modern,
            whisper_model: String::new(),
            chunk_seconds: default_chunk_seconds(),
        }
    }
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            mode: QualityMode::Copy,
            preset: default_preset(),
            crf: default_crf(),
        }
    }
}

impl Default for StandbyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_first_publish: true,
            video: default_standby_video(),
            audio: default_standby_audio(),
            width: default_standby_width(),
            height: default_standby_height(),
            fps: default_standby_fps(),
            delay_secs: default_standby_delay(),
        }
    }
}

impl StandbyConfig {
    pub fn resolve_video(&self, config_path: &Path) -> Option<PathBuf> {
        resolve_asset(config_path, &self.video)
    }

    pub fn resolve_audio(&self, config_path: &Path) -> Option<PathBuf> {
        resolve_asset(config_path, &self.audio)
    }
}

fn resolve_asset(config_path: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim();
    if rel.is_empty() {
        return None;
    }
    let p = Path::new(rel);
    if p.is_file() {
        return Some(p.to_path_buf());
    }
    if let Some(dir) = config_path.parent() {
        let c = dir.join(p);
        if c.is_file() {
            return Some(c);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join(p);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

fn rewrite_bind_host(bind: &str, host: &str) -> Result<String> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("cannot parse bind {bind}"))?;
    let port = addr.port();
    if host.contains(':') && !host.starts_with('[') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
}

fn destination_line(label: &str, platform: Option<(bool, &str)>, url: Option<String>) -> String {
    let pad = format!("{label:<19}");
    match url {
        Some(u) => format!("{pad}{}", crate::redact::redact_url(&u)),
        None => match platform {
            Some((true, key)) if key.trim().is_empty() => {
                format!("{pad}enabled, but stream_key is empty")
            }
            _ => format!("{pad}(disabled)"),
        },
    }
}

fn client_dial_bind(bind: &str) -> String {
    match bind.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_unspecified() => {
            if addr.is_ipv6() {
                format!("[::1]:{}", addr.port())
            } else {
                format!("127.0.0.1:{}", addr.port())
            }
        }
        _ => bind.to_string(),
    }
}

impl Config {
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate_struct()?;
        Ok(cfg)
    }

    /// CLI overrides. `--host` rewrites the IP on both binds (ports stay).
    /// `--ingest-bind` / `--control-bind` replace the full address and win over `--host`.
    pub fn apply_bind_overrides(
        &mut self,
        host: Option<&str>,
        ingest_bind: Option<&str>,
        control_bind: Option<&str>,
    ) -> Result<()> {
        if let Some(h) = host {
            let h = h.trim();
            if h.is_empty() {
                bail!("--host must not be empty");
            }
            self.ingest.bind = rewrite_bind_host(&self.ingest.bind, h)?;
            self.control.bind = rewrite_bind_host(&self.control.bind, h)?;
        }
        if let Some(b) = ingest_bind {
            self.ingest.bind = b.trim().to_string();
        }
        if let Some(b) = control_bind {
            self.control.bind = b.trim().to_string();
        }
        self.validate_struct()
    }

    /// Address clients should dial. `0.0.0.0` / `::` become loopback, same port.
    pub fn client_control_bind(&self) -> String {
        client_dial_bind(&self.control.bind)
    }

    /// Discovery order: explicit path, `ECHOCHAMBER_CONFIG`, `./config.toml`,
    /// `~/.config/echochamber/config.toml`.
    pub fn discover(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
        if let Some(p) = explicit {
            return Ok((Self::load_from_path(p)?, p.to_path_buf()));
        }
        if let Ok(env_path) = std::env::var("ECHOCHAMBER_CONFIG") {
            let p = PathBuf::from(env_path);
            return Ok((Self::load_from_path(&p)?, p));
        }
        let cwd = PathBuf::from("config.toml");
        if cwd.is_file() {
            return Ok((Self::load_from_path(&cwd)?, cwd));
        }
        if let Some(p) = user_config_path() {
            if p.is_file() {
                return Ok((Self::load_from_path(&p)?, p));
            }
        }
        bail!(
            "no config found. Run `echochamber init` or pass --config. Looked at ./config.toml and ~/.config/echochamber/config.toml"
        );
    }

    pub fn validate_struct(&self) -> Result<()> {
        self.ingest.bind.parse::<SocketAddr>().with_context(|| {
            format!("ingest.bind is not a socket address: {}", self.ingest.bind)
        })?;
        self.control.bind.parse::<SocketAddr>().with_context(|| {
            format!(
                "control.bind is not a socket address: {}",
                self.control.bind
            )
        })?;
        if self.ingest.app.trim().is_empty() {
            bail!("ingest.app must not be empty");
        }
        if self.ingest.stream_key.trim().is_empty() {
            bail!("ingest.stream_key must not be empty");
        }
        Ok(())
    }

    pub fn youtube_publish_url(&self) -> Option<String> {
        let yt = self.platforms.youtube.as_ref()?;
        if !yt.enabled || yt.stream_key.trim().is_empty() {
            return None;
        }
        let base = yt
            .ingest_base
            .as_deref()
            .unwrap_or(DEFAULT_YOUTUBE_INGEST)
            .trim_end_matches('/');
        Some(format!("{base}/{}", yt.stream_key.trim()))
    }

    pub fn x_publish_url(&self) -> Option<String> {
        let x = self.platforms.x.as_ref()?;
        if !x.enabled || x.stream_key.trim().is_empty() {
            return None;
        }
        let base = x
            .ingest_base
            .as_deref()
            .unwrap_or(DEFAULT_X_INGEST)
            .trim_end_matches('/');
        Some(format!("{base}/{}", x.stream_key.trim()))
    }

    pub fn twitch_publish_url(&self) -> Option<String> {
        let tw = self.platforms.twitch.as_ref()?;
        if !tw.enabled || tw.stream_key.trim().is_empty() {
            return None;
        }
        let base = tw
            .ingest_base
            .as_deref()
            .unwrap_or(DEFAULT_TWITCH_INGEST)
            .trim_end_matches('/');
        Some(format!("{base}/{}", tw.stream_key.trim()))
    }

    pub fn configured_destinations(&self) -> Vec<Destination> {
        let mut out = Vec::new();
        if let Some(url) = self.youtube_publish_url() {
            out.push(Destination {
                name: "youtube".into(),
                url,
            });
        }
        if let Some(url) = self.x_publish_url() {
            out.push(Destination {
                name: "x".into(),
                url,
            });
        }
        if let Some(url) = self.twitch_publish_url() {
            out.push(Destination {
                name: "twitch".into(),
                url,
            });
        }
        out
    }

    pub fn ingest_host_port(&self) -> (String, u16) {
        let addr: SocketAddr = self
            .ingest
            .bind
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:1935".parse().unwrap());
        let host = if addr.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            addr.ip().to_string()
        };
        (host, addr.port())
    }

    /// tcUrl OBS uses: rtmp://host:port/app  (stream key is a separate playpath)
    pub fn local_rtmp_tc_url(&self) -> String {
        let (host, port) = self.ingest_host_port();
        format!(
            "rtmp://{}:{}/{}",
            host,
            port,
            self.ingest.app.trim_matches('/')
        )
    }

    /// Display URL including playpath. Do not pass this whole string to ffmpeg -i:
    /// ffmpeg would treat `app/key` as the RTMP application name, so a play of
    /// `.../echochamber/live` fails with "Stream not found: live" when OBS
    /// published as app=`echochamber` + stream=`live`.
    pub fn local_rtmp_url(&self) -> String {
        format!(
            "{}/{}",
            self.local_rtmp_tc_url(),
            self.ingest.stream_key.trim_matches('/')
        )
    }

    pub fn masked_summary(&self) -> String {
        let mut lines = vec![
            format!("ingest.bind        {}", self.ingest.bind),
            format!("ingest.app         {}", self.ingest.app),
            format!("ingest.stream_key  {}", redact_key(&self.ingest.stream_key)),
            format!("control.bind       {}", self.control.bind),
            format!("local play/publish {}", self.local_rtmp_url()),
        ];
        lines.push(destination_line(
            "youtube",
            self.platforms.youtube.as_ref().map(|p| (p.enabled, p.stream_key.as_str())),
            self.youtube_publish_url(),
        ));
        lines.push(destination_line(
            "x",
            self.platforms.x.as_ref().map(|p| (p.enabled, p.stream_key.as_str())),
            self.x_publish_url(),
        ));
        lines.push(destination_line(
            "twitch",
            self.platforms
                .twitch
                .as_ref()
                .map(|p| (p.enabled, p.stream_key.as_str())),
            self.twitch_publish_url(),
        ));
        lines.push(format!(
            "subtitles          enabled={} burn_in={} sidecar={} lang={} style={:?}",
            self.subtitles.enabled,
            self.subtitles.burn_in,
            self.subtitles.sidecar,
            self.subtitles.language,
            self.subtitles.style
        ));
        lines.push(format!("quality.mode       {:?}", self.quality.mode));
        lines.push(format!(
            "standby            enabled={} after_first_publish={} {}x{}@{} {video} {audio}",
            self.standby.enabled,
            self.standby.after_first_publish,
            self.standby.width,
            self.standby.height,
            self.standby.fps,
            video = self.standby.video,
            audio = if self.standby.audio.is_empty() {
                "(silence)"
            } else {
                self.standby.audio.as_str()
            }
        ));
        lines.join("\n")
    }
}

#[derive(Debug, Clone)]
pub struct Destination {
    pub name: String,
    pub url: String,
}

pub fn user_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "echochamber")
        .map(|d| d.config_dir().join("config.toml"))
}

pub fn write_init_template(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    if path.exists() {
        bail!("{} already exists — refusing to overwrite", path.display());
    }
    fs::write(path, INIT_TEMPLATE)?;
    Ok(())
}

pub const INIT_TEMPLATE: &str = r#"# echochamber
#
# OBS sends one stream here. This program forwards it to YouTube, X, and Twitch.
#
# OBS settings:
#   Server:      rtmp://127.0.0.1:1935/echochamber
#   Stream key:  live
#
# The program reads the first config file it finds:
#   --config PATH
#   $ECHOCHAMBER_CONFIG
#   ./config.toml
#   ~/.config/echochamber/config.toml
#
# Optional programs, if the normal ones on your PATH are not the ones you want:
#   ECHOCHAMBER_FFMPEG_BIN    ffmpeg (needed with libass only if burn_in is true)
#   ECHOCHAMBER_WHISPER_BIN   whisper.cpp's whisper-cli, for live captions

[ingest]
# Where OBS connects. This computer only.
# Another machine on your network: bind = "0.0.0.0:1935"
# or start with: echochamber serve --host 0.0.0.0
bind = "127.0.0.1:1935"
app = "echochamber"
# Must match the stream key typed into OBS.
stream_key = "live"

[control]
# Local web address used by: status, reload, stop, push
# Open it on the network for this run with: serve --control-bind 0.0.0.0:8080
bind = "127.0.0.1:8080"

[platforms.youtube]
enabled = false
# YouTube Studio → Go live → Stream key
stream_key = ""
# ingest_base = "rtmp://a.rtmp.youtube.com/live2"
# Not used for sending video. Only for a YouTube captions API, if you add one later.
# client_id = ""
# client_secret = ""
# refresh_token = ""

[platforms.x]
enabled = false
# studio.x.com/producer → create an RTMP source → copy the key
# After this program connects, also create a broadcast in X and go live.
stream_key = ""
# ingest_base = "rtmp://va.pscp.tv:80/x"

[platforms.twitch]
enabled = false
# twitch.tv → Creator Dashboard → Settings → Stream → Primary Stream Key
stream_key = ""
# ingest_base = "rtmp://live.twitch.tv/app"
# ingest_base = "rtmps://live.twitch.tv:443/app"

[subtitles]
enabled = false
# auto, en, or ru
language = "auto"
# true paints words onto the picture. Needs ffmpeg built with libass.
# The usual Homebrew ffmpeg cannot do this. Leave false and use the subtitle files.
burn_in = false
# true writes echochamber_live.srt and echochamber_live.ass next to the program.
sidecar = true
# modern, minimal, or broadcast
style = "modern"
# Path to a whisper.cpp model file, such as ggml-small.bin
whisper_model = ""
# Seconds of speech sent to Whisper at a time.
chunk_seconds = 8

[quality]
# copy sends OBS's picture through unchanged. reencode compresses it again with libx264.
mode = "copy"
preset = "veryfast"
crf = 23

[standby]
# true shows the waiting video while OBS is not streaming.
enabled = false
# true waits until you have gone live once, then covers a dropout.
# false also shows the waiting video before the first stream.
after_first_publish = true
# Looping picture, and optional music. Leave audio empty for silence.
# Both are resized to width x height at fps. Your live stream is not resized.
video = "assets/standby.mp4"
audio = "assets/standby.m4a"
width = 1920
height = 1080
fps = 30
# Seconds to wait after OBS stops, so a short glitch does not flash the waiting video.
delay_secs = 2
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_and_x_urls() {
        let mut cfg = Config::default();
        cfg.platforms.youtube = Some(YoutubePlatform {
            enabled: true,
            stream_key: "yt-key-1234".into(),
            ingest_base: None,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        });
        cfg.platforms.x = Some(XPlatform {
            enabled: true,
            stream_key: "xkey9999".into(),
            ingest_base: None,
        });
        cfg.platforms.twitch = Some(TwitchPlatform {
            enabled: true,
            stream_key: "live_twitch_key99".into(),
            ingest_base: None,
        });
        assert_eq!(
            cfg.youtube_publish_url().unwrap(),
            "rtmp://a.rtmp.youtube.com/live2/yt-key-1234"
        );
        assert_eq!(
            cfg.x_publish_url().unwrap(),
            "rtmp://va.pscp.tv:80/x/xkey9999"
        );
        assert_eq!(
            cfg.twitch_publish_url().unwrap(),
            "rtmp://live.twitch.tv/app/live_twitch_key99"
        );
        assert_eq!(cfg.configured_destinations().len(), 3);
    }

    #[test]
    fn disabled_platform_is_skipped() {
        let mut cfg = Config::default();
        cfg.platforms.youtube = Some(YoutubePlatform {
            enabled: false,
            stream_key: "secret".into(),
            ingest_base: None,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        });
        assert!(cfg.youtube_publish_url().is_none());
        assert!(cfg.configured_destinations().is_empty());
    }

    #[test]
    fn template_parses() {
        let cfg: Config = toml::from_str(INIT_TEMPLATE).unwrap();
        cfg.validate_struct().unwrap();
        assert_eq!(cfg.ingest.app, "echochamber");
        assert_eq!(cfg.ingest.stream_key, "live");
        assert!(!cfg.standby.enabled);
        assert!(cfg.standby.after_first_publish);
        assert_eq!(cfg.standby.width, 1920);
        assert_eq!(cfg.standby.fps, 30);
    }

    #[test]
    fn missing_standby_table_is_opt_in_off() {
        let cfg: Config = toml::from_str(
            r#"
[ingest]
bind = "127.0.0.1:1935"
"#,
        )
        .unwrap();
        assert!(!cfg.standby.enabled);
        assert!(cfg.standby.after_first_publish);
        assert_eq!(cfg.standby.video, "assets/standby.mp4");
    }

    #[test]
    fn local_rtmp_url_rewrites_wildcard() {
        let mut cfg = Config::default();
        cfg.ingest.bind = "0.0.0.0:1935".into();
        assert_eq!(
            cfg.local_rtmp_url(),
            "rtmp://127.0.0.1:1935/echochamber/live"
        );
        assert_eq!(cfg.local_rtmp_tc_url(), "rtmp://127.0.0.1:1935/echochamber");
    }

    #[test]
    fn enabled_platform_without_a_key_is_not_called_disabled() {
        let cfg: Config = toml::from_str(
            r#"
[platforms.youtube]
enabled = true
stream_key = ""

[platforms.x]
enabled = false
stream_key = "xkey9999"
"#,
        )
        .unwrap();
        let summary = cfg.masked_summary();
        assert!(
            summary.contains("youtube            enabled, but stream_key is empty"),
            "{summary}"
        );
        assert!(summary.contains("x                  (disabled)"), "{summary}");
        assert!(cfg.configured_destinations().is_empty());
    }

    #[test]
    fn standby_video_resolves_beside_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("wait.mp4");
        std::fs::write(&clip, b"not a real mp4").unwrap();
        let cfg_path = dir.path().join("mine.toml");
        let mut standby = StandbyConfig::default();
        standby.video = "wait.mp4".into();
        assert!(standby.resolve_video(Path::new("config.toml")).is_none());
        assert_eq!(standby.resolve_video(&cfg_path).unwrap(), clip);
    }

    #[test]
    fn bind_overrides_host_then_full_addr() {
        let mut cfg = Config::default();
        cfg.apply_bind_overrides(Some("0.0.0.0"), None, None)
            .unwrap();
        assert_eq!(cfg.ingest.bind, "0.0.0.0:1935");
        assert_eq!(cfg.control.bind, "0.0.0.0:8080");
        assert_eq!(cfg.client_control_bind(), "127.0.0.1:8080");
        cfg.apply_bind_overrides(Some("10.0.0.8"), Some("0.0.0.0:1936"), None)
            .unwrap();
        assert_eq!(cfg.ingest.bind, "0.0.0.0:1936");
        assert_eq!(cfg.control.bind, "10.0.0.8:8080");
    }
}
