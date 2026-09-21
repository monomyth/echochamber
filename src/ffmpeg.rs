use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Result;

use crate::config::{Config, QualityMode};

pub fn ffmpeg_bin() -> PathBuf {
    std::env::var_os("ECHOCHAMBER_FFMPEG_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

pub fn whisper_bin() -> PathBuf {
    std::env::var_os("ECHOCHAMBER_WHISPER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("whisper-cli"))
}

pub fn ffmpeg_has_burnin_filter() -> bool {
    let bin = ffmpeg_bin();
    let output = std::process::Command::new(&bin)
        .args(["-hide_banner", "-filters"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines().any(|l| {
                let cols: Vec<_> = l.split_whitespace().collect();
                cols.iter().any(|c| *c == "ass" || *c == "subtitles")
            })
        }
        Err(_) => false,
    }
}

/// Burn-in filter. Prefer `ass=filename=...` (no force_style quoting).
pub fn build_burn_filter_if_needed(ass_path: &Path, burn_in: bool) -> Option<String> {
    if !burn_in {
        return None;
    }
    Some(format!("ass=filename={}", ass_path.display()))
}

pub struct PusherSpec {
    pub input: String,
    pub rtmp_app: String,
    pub rtmp_playpath: String,
    pub output: String,
    pub burn_filter: Option<String>,
    pub mode: QualityMode,
    pub preset: String,
    pub crf: u8,
}

impl PusherSpec {
    pub fn from_config(cfg: &Config, output: String, ass_path: &Path) -> Self {
        Self::from_published(cfg, output, ass_path, None)
    }

    pub fn from_published(
        cfg: &Config,
        output: String,
        ass_path: &Path,
        published: Option<&crate::state::PublishedStream>,
    ) -> Self {
        let app = published
            .map(|p| p.app.as_str())
            .unwrap_or(cfg.ingest.app.trim_matches('/'))
            .to_string();
        let playpath = published
            .map(|p| p.stream_key.as_str())
            .unwrap_or(cfg.ingest.stream_key.trim_matches('/'))
            .to_string();
        let (host, port) = cfg.ingest_host_port();
        Self {
            // Keep app/playpath out of the URL so ffmpeg cannot re-split them.
            input: format!("rtmp://{host}:{port}"),
            rtmp_app: app,
            rtmp_playpath: playpath,
            output,
            burn_filter: build_burn_filter_if_needed(ass_path, cfg.subtitles.burn_in),
            mode: cfg.quality.mode,
            preset: cfg.quality.preset.clone(),
            crf: cfg.quality.crf,
        }
    }

    /// argv after the ffmpeg binary. Reconnect flags sit immediately before `-f flv <url>`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "warning".into(),
            "-progress".into(),
            "pipe:1".into(),
            "-stats_period".into(),
            "0.2".into(),
            "-rw_timeout".into(),
            "15000000".into(),
        ];
        if self.mode == QualityMode::Copy && self.burn_filter.is_none() {
            // Copy path: don't analyze or invent timestamps. That work is per
            // destination and dominates CPU on a 2K ingest.
            args.extend([
                "-fflags".into(),
                "+nobuffer+discardcorrupt".into(),
                "-probesize".into(),
                "32768".into(),
                "-analyzeduration".into(),
                "0".into(),
            ]);
        } else {
            args.extend(["-fflags".into(), "+genpts+discardcorrupt".into()]);
        }
        args.extend(rtmp_input_opts(
            &self.input,
            &self.rtmp_app,
            &self.rtmp_playpath,
        ));
        match self.mode {
            QualityMode::Copy if self.burn_filter.is_none() => {
                args.push("-c".into());
                args.push("copy".into());
            }
            QualityMode::Copy => {
                // Burn-in requires a video encode; copy audio.
                args.extend([
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    self.preset.clone(),
                    "-crf".into(),
                    self.crf.to_string(),
                    "-c:a".into(),
                    "copy".into(),
                ]);
            }
            QualityMode::Reencode => {
                args.extend([
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    self.preset.clone(),
                    "-crf".into(),
                    self.crf.to_string(),
                    "-c:a".into(),
                    "aac".into(),
                    "-b:a".into(),
                    "128k".into(),
                    "-g".into(),
                    "60".into(),
                ]);
            }
        }
        if let Some(vf) = &self.burn_filter {
            args.push("-vf".into());
            args.push(vf.clone());
        }
        // Do not pass -reconnect: ffmpeg would keep the dest socket while OBS
        // restarts, then a new pusher would hit the same ingest (conflicting streams).
        if self.mode == QualityMode::Copy && self.burn_filter.is_none() {
            args.extend(["-muxdelay".into(), "0".into(), "-muxpreload".into(), "0".into()]);
        } else {
            args.push("-avoid_negative_ts".into());
            args.push("make_zero".into());
        }
        args.push("-f".into());
        args.push("flv".into());
        args.push("-flvflags".into());
        args.push("no_duration_filesize".into());
        args.push(self.output.clone());
        args
    }
}

/// Loop a local slate to a dest. Video is ping-pong or otherwise already looping;
/// we re-encode to the configured size/fps so dests get a live-shaped H.264+AAC.
pub fn standby_args(
    cfg: &Config,
    video: &Path,
    audio: Option<&Path>,
    output: &str,
) -> Vec<String> {
    let w = cfg.standby.width.max(16);
    let h = cfg.standby.height.max(16);
    let fps = cfg.standby.fps.max(1);
    let g = (fps * 2).max(2);
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-stats_period".into(),
        "0.2".into(),
        "-re".into(),
        "-stream_loop".into(),
        "-1".into(),
        "-i".into(),
        video.display().to_string(),
    ];
    match audio {
        Some(a) => {
            args.extend([
                "-re".into(),
                "-stream_loop".into(),
                "-1".into(),
                "-i".into(),
                a.display().to_string(),
            ]);
        }
        None => {
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "anullsrc=channel_layout=stereo:sample_rate=48000".into(),
            ]);
        }
    }
    args.extend([
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "1:a:0".into(),
        "-vf".into(),
        format!("scale={w}:{h}:flags=fast_bilinear,fps={fps},format=yuv420p"),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        cfg.quality.preset.clone(),
        "-tune".into(),
        "zerolatency".into(),
        "-crf".into(),
        cfg.quality.crf.to_string(),
        "-profile:v".into(),
        "main".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-bf".into(),
        "0".into(),
        "-g".into(),
        g.to_string(),
        "-c:a".into(),
        "aac".into(),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "2".into(),
        "-b:a".into(),
        "128k".into(),
    ]);
    args.push("-f".into());
    args.push("flv".into());
    args.push("-flvflags".into());
    args.push("no_duration_filesize".into());
    args.push(output.to_string());
    args
}

/// One x264 encode, fanned out with the tee muxer. `onfail=ignore` so one
/// destination dropping does not kill the others.
pub fn standby_tee_args(
    cfg: &Config,
    video: &Path,
    audio: Option<&Path>,
    outputs: &[String],
) -> Vec<String> {
    if outputs.len() <= 1 {
        return standby_args(
            cfg,
            video,
            audio,
            outputs.first().map(String::as_str).unwrap_or("pipe:1"),
        );
    }
    let mut args = standby_args(cfg, video, audio, "unused");
    // Drop the single-file muxer tail; replace with tee.
    let f = args.iter().rposition(|a| a == "-f").expect("-f");
    args.truncate(f);
    let spec = outputs
        .iter()
        .map(|url| {
            format!(
                "[f=flv:flvflags=no_duration_filesize:onfail=ignore]{}",
                tee_escape(url)
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    args.push("-f".into());
    args.push("tee".into());
    args.push(spec);
    args
}

fn tee_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '\\' | ':' | '|' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn is_rtmp_url(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    u.starts_with("rtmp://") || u.starts_with("rtmps://")
}

/// Force ffmpeg's RTMP client to use the same app/playpath split the encoder used.
/// `tc_url` must be `rtmp://host:port` with no path — ffmpeg 9 re-parses paths on -i
/// and would otherwise ignore -rtmp_app.
pub fn rtmp_input_opts(tc_url: &str, app: &str, playpath: &str) -> Vec<String> {
    let tcurl = format!("{}/{}", tc_url.trim_end_matches('/'), app.trim_matches('/'));
    vec![
        "-rtmp_live".into(),
        "live".into(),
        "-rtmp_app".into(),
        app.to_string(),
        "-rtmp_playpath".into(),
        playpath.to_string(),
        "-rtmp_tcurl".into(),
        tcurl,
        "-i".into(),
        tc_url.to_string(),
    ]
}

pub fn probe_ffmpeg() -> Result<String> {
    let bin = ffmpeg_bin();
    let output = std::process::Command::new(&bin)
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let line = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("ffmpeg")
                .to_string();
            Ok(format!("{} ({line})", bin.display()))
        }
        Ok(o) => anyhow::bail!(
            "ffmpeg at {} failed: {}",
            bin.display(),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => anyhow::bail!("ffmpeg not found at {}: {e}", bin.display()),
    }
}

pub fn asr_extract_args(
    cfg: &Config,
    published: Option<&crate::state::PublishedStream>,
    wav: &Path,
    seconds: u64,
) -> Vec<String> {
    let spec = PusherSpec::from_published(cfg, String::new(), Path::new(""), published);
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-rw_timeout".into(),
        "8000000".into(),
        "-t".into(),
        seconds.to_string(),
    ];
    args.extend(rtmp_input_opts(
        &spec.input,
        &spec.rtmp_app,
        &spec.rtmp_playpath,
    ));
    args.extend([
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "16000".into(),
        "-y".into(),
        wav.display().to_string(),
    ]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reconnect_flags_are_before_output_not_input() {
        let spec = PusherSpec {
            input: "rtmp://127.0.0.1:1935".into(),
            rtmp_app: "echochamber".into(),
            rtmp_playpath: "live".into(),
            output: "rtmp://a.rtmp.youtube.com/live2/key".into(),
            burn_filter: None,
            mode: QualityMode::Copy,
            preset: "veryfast".into(),
            crf: 23,
        };
        let args = spec.args();
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        let f_pos = args.iter().position(|a| a == "-f").unwrap();
        assert!(i_pos < f_pos);
        assert!(!args.iter().any(|a| a == "-reconnect"));
        assert_eq!(args[f_pos + 1], "flv");
        assert!(args.windows(2).any(|w| w == ["-progress", "pipe:1"]));
        assert!(args.windows(2).any(|w| w == ["-stats_period", "0.2"]));
        assert!(args.windows(2).any(|w| w == ["-rtmp_app", "echochamber"]));
        assert!(args.windows(2).any(|w| w == ["-rtmp_playpath", "live"]));
        assert_eq!(args[i_pos + 1], "rtmp://127.0.0.1:1935");
        assert!(args
            .windows(2)
            .any(|w| w == ["-rtmp_tcurl", "rtmp://127.0.0.1:1935/echochamber"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-fflags", "+nobuffer+discardcorrupt"]));
        assert!(args.windows(2).any(|w| w == ["-analyzeduration", "0"]));
        assert!(args.windows(2).any(|w| w == ["-muxdelay", "0"]));
        let fflags_at = args.iter().position(|a| a == "-fflags").unwrap();
        assert!(fflags_at < i_pos, "nobuffer is an input flag");
        assert_eq!(args[f_pos + 1], "flv");
        assert_eq!(args[f_pos + 2], "-flvflags");
        assert_eq!(args[f_pos + 3], "no_duration_filesize");
        assert_eq!(args.last().unwrap(), &spec.output);
    }

    #[test]
    fn file_output_skips_reconnect() {
        let spec = PusherSpec {
            input: "rtmp://127.0.0.1:1935".into(),
            rtmp_app: "echochamber".into(),
            rtmp_playpath: "live".into(),
            output: "/tmp/out.flv".into(),
            burn_filter: None,
            mode: QualityMode::Copy,
            preset: "veryfast".into(),
            crf: 23,
        };
        let args = spec.args();
        assert!(!args.iter().any(|a| a == "-reconnect"));
    }

    #[test]
    fn burn_filter_is_ass_filename_without_force_style() {
        let path = PathBuf::from("echochamber_live.ass");
        let f = build_burn_filter_if_needed(&path, true).unwrap();
        assert_eq!(f, "ass=filename=echochamber_live.ass");
        assert!(!f.contains("force_style"));
        assert!(build_burn_filter_if_needed(&path, false).is_none());
    }

    #[test]
    fn standby_args_loop_and_scale() {
        let cfg = Config::default();
        let args = standby_args(
            &cfg,
            Path::new("assets/standby.mp4"),
            Some(Path::new("assets/standby.m4a")),
            "rtmp://live.twitch.tv/app/key",
        );
        assert!(args.windows(2).any(|w| w == ["-stream_loop", "-1"]));
        assert!(args.iter().filter(|a| *a == "-re").count() >= 2);
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-vf" && w[1].contains("fast_bilinear")));
        let tee = standby_tee_args(
            &cfg,
            Path::new("assets/standby.mp4"),
            None,
            &[
                "rtmp://a.example/live/one".into(),
                "rtmp://b.example/live/two".into(),
            ],
        );
        assert!(tee.windows(2).any(|w| w == ["-f", "tee"]));
        let spec = tee.last().unwrap();
        assert!(spec.contains("onfail=ignore"));
        assert!(spec.contains(r"rtmp\://"));
        assert!(!spec.contains("rtmp://"));
        assert!(args.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert_eq!(args.last().unwrap(), "rtmp://live.twitch.tv/app/key");
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        assert!(
            !args[..i_pos].iter().any(|a| a == "-c"),
            "encode flags must not precede inputs"
        );
    }
}
