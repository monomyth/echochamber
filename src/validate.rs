use std::path::Path;

use anyhow::{bail, Result};

use crate::config::Config;
use crate::ffmpeg::{ffmpeg_bin, ffmpeg_has_burnin_filter, probe_ffmpeg, whisper_bin, PusherSpec};
use crate::pusher::sidecar_paths;
use crate::redact::redact_cmd;

/// Config problems `validate` should refuse to call ready.
/// Does not look at whether ffmpeg itself is installed.
pub fn problems(cfg: &Config, config_path: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    note_empty_key(
        &mut problems,
        "youtube",
        cfg.platforms
            .youtube
            .as_ref()
            .map(|p| (p.enabled, p.stream_key.as_str())),
    );
    note_empty_key(
        &mut problems,
        "x",
        cfg.platforms
            .x
            .as_ref()
            .map(|p| (p.enabled, p.stream_key.as_str())),
    );
    note_empty_key(
        &mut problems,
        "twitch",
        cfg.platforms.twitch.as_ref().map(|p| (p.enabled, p.stream_key.as_str())),
    );
    if cfg.standby.enabled && cfg.standby.resolve_video(config_path).is_none() {
        problems.push(format!(
            "standby is enabled but the video file was not found: {}",
            cfg.standby.video
        ));
    }
    if cfg.subtitles.enabled {
        let model = cfg.subtitles.whisper_model.trim();
        if model.is_empty() {
            problems.push("subtitles are enabled but whisper_model is empty".into());
        } else if !model_exists(config_path, model) {
            problems.push(format!(
                "subtitles are enabled but the whisper model was not found: {model}"
            ));
        }
    }
    problems
}

fn note_empty_key(problems: &mut Vec<String>, name: &str, platform: Option<(bool, &str)>) {
    if let Some((true, key)) = platform {
        if key.trim().is_empty() {
            problems.push(format!("{name} is enabled but stream_key is empty"));
        }
    }
}

fn model_exists(config_path: &Path, model: &str) -> bool {
    let path = Path::new(model);
    if path.is_file() {
        return true;
    }
    config_path
        .parent()
        .is_some_and(|dir| dir.join(path).is_file())
}

/// ffmpeg command for one destination, with the stream key hidden.
pub fn masked_ffmpeg_line(cfg: &Config, output: &str) -> String {
    let (_, ass) = sidecar_paths();
    let spec = PusherSpec::from_config(cfg, output.to_string(), &ass);
    redact_cmd(&ffmpeg_bin().display().to_string(), &spec.args())
}

pub fn run(cfg: &Config, config_path: &Path, verbose: bool) -> Result<()> {
    cfg.validate_struct()?;
    println!("{}", cfg.masked_summary());
    println!();

    let mut problems = problems(cfg, config_path);
    match probe_ffmpeg() {
        Ok(s) => println!("ffmpeg             {s}"),
        Err(e) => {
            println!("ffmpeg             ERROR: {e}");
            problems.push(format!("ffmpeg: {e}"));
        }
    }
    let burn = ffmpeg_has_burnin_filter();
    println!(
        "ffmpeg burn-in     {} ({})",
        if burn {
            "yes"
        } else {
            "NO ass/subtitles filter"
        },
        ffmpeg_bin().display()
    );
    if cfg.subtitles.burn_in && !burn {
        println!(
            "  hint: stock Homebrew ffmpeg often lacks libass.\n  \
             brew tap homebrew-ffmpeg/ffmpeg && brew install homebrew-ffmpeg/ffmpeg/ffmpeg\n  \
             then: export ECHOCHAMBER_FFMPEG_BIN=$(brew --prefix homebrew-ffmpeg/ffmpeg/ffmpeg)/bin/ffmpeg"
        );
    }

    if cfg.subtitles.enabled {
        println!("whisper-cli        {}", whisper_bin().display());
        let model = cfg.subtitles.whisper_model.trim();
        if model.is_empty() {
            println!("whisper model      (not set)");
        } else if model_exists(config_path, model) {
            println!("whisper model      {model}");
        } else {
            println!("whisper model      MISSING {model}");
        }
    }

    if cfg.standby.enabled {
        match cfg.standby.resolve_video(config_path) {
            Some(p) => println!("standby video      {}", p.display()),
            None => println!("standby video      MISSING {}", cfg.standby.video),
        }
        if !cfg.standby.audio.trim().is_empty() {
            match cfg.standby.resolve_audio(config_path) {
                Some(p) => println!("standby audio      {}", p.display()),
                None => println!(
                    "standby audio      MISSING {} (silence)",
                    cfg.standby.audio
                ),
            }
        }
        println!(
            "standby encode     {}x{} @ {} fps after_first_publish={}",
            cfg.standby.width,
            cfg.standby.height,
            cfg.standby.fps,
            cfg.standby.after_first_publish
        );
    }

    let dests = cfg.configured_destinations();
    if dests.is_empty() {
        println!("platforms          none");
    } else {
        println!("platforms          {}", dests.len());
    }

    if verbose {
        println!("\nffmpeg commands (stream keys hidden):");
        if dests.is_empty() {
            println!(
                "  mock     {}",
                masked_ffmpeg_line(cfg, "/tmp/echochamber-mock.flv")
            );
        }
        for d in &dests {
            println!("  {:<8} {}", d.name, masked_ffmpeg_line(cfg, &d.url));
        }
    }

    if !problems.is_empty() {
        println!();
        for problem in &problems {
            println!("problem: {problem}");
        }
        bail!("{} problem(s); not ready", problems.len());
    }

    println!("\nready.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, StandbyConfig, SubtitlesConfig, YoutubePlatform};

    #[test]
    fn empty_key_and_missing_waiting_video_are_problems() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.platforms.youtube = Some(YoutubePlatform {
            enabled: true,
            stream_key: "  ".into(),
            ingest_base: None,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        });
        cfg.standby = StandbyConfig {
            enabled: true,
            video: "missing.mp4".into(),
            ..StandbyConfig::default()
        };
        cfg.subtitles = SubtitlesConfig {
            enabled: true,
            whisper_model: String::new(),
            ..SubtitlesConfig::default()
        };
        let problems = problems(&cfg, &cfg_path);
        assert!(problems.iter().any(|p| p.contains("youtube is enabled")));
        assert!(problems.iter().any(|p| p.contains("video file was not found")));
        assert!(problems.iter().any(|p| p.contains("whisper_model is empty")));
    }

    #[test]
    fn waiting_video_beside_the_config_is_not_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("wait.mp4");
        std::fs::write(&clip, b"x").unwrap();
        let cfg_path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.standby = StandbyConfig {
            enabled: true,
            video: "wait.mp4".into(),
            audio: String::new(),
            ..StandbyConfig::default()
        };
        assert!(problems(&cfg, &cfg_path).is_empty(), "{:?}", problems(&cfg, &cfg_path));
    }

    #[test]
    fn verbose_command_hides_the_stream_key() {
        let mut cfg = Config::default();
        cfg.platforms.youtube = Some(YoutubePlatform {
            enabled: true,
            stream_key: "super-secret-key".into(),
            ingest_base: None,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        });
        let line = masked_ffmpeg_line(&cfg, &cfg.youtube_publish_url().unwrap());
        assert!(!line.contains("super-secret"));
        assert!(line.contains("***-key"), "{line}");
    }
}
