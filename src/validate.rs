use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::ffmpeg::{ffmpeg_bin, ffmpeg_has_burnin_filter, probe_ffmpeg, whisper_bin, PusherSpec};
use crate::pusher::sidecar_paths;

pub fn run(cfg: &Config, verbose: bool) -> Result<()> {
    cfg.validate_struct()?;
    println!("{}", cfg.masked_summary());
    println!();

    match probe_ffmpeg() {
        Ok(s) => println!("ffmpeg             {s}"),
        Err(e) => println!("ffmpeg             ERROR: {e}"),
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
        let model = Path::new(&cfg.subtitles.whisper_model);
        if cfg.subtitles.whisper_model.trim().is_empty() {
            println!("whisper model      (not set)");
        } else if model.is_file() {
            println!("whisper model      {}", model.display());
        } else {
            println!("whisper model      MISSING {}", model.display());
        }
    }

    if cfg.standby.enabled {
        match cfg.standby.resolve_video(Path::new("config.toml")) {
            Some(p) => println!("standby video     {}", p.display()),
            None => println!(
                "standby video     MISSING {} (slate disabled until the file exists)",
                cfg.standby.video
            ),
        }
        if !cfg.standby.audio.trim().is_empty() {
            match cfg.standby.resolve_audio(Path::new("config.toml")) {
                Some(p) => println!("standby audio     {}", p.display()),
                None => println!("standby audio     MISSING {} (silence)", cfg.standby.audio),
            }
        }
        println!(
            "standby encode    {}x{} @ {} fps after_first_publish={} (on the fly)",
            cfg.standby.width,
            cfg.standby.height,
            cfg.standby.fps,
            cfg.standby.after_first_publish
        );
    }

    let dests = cfg.configured_destinations();
    if dests.is_empty() {
        println!("platforms          none (relay-only; publish to local RTMP to test ASR)");
    } else {
        println!("platforms          {}", dests.len());
    }

    if verbose {
        let (_, ass) = sidecar_paths();
        println!("\nffmpeg templates:");
        if dests.is_empty() {
            let spec = PusherSpec::from_config(cfg, "/tmp/echochamber-mock.flv".into(), &ass);
            println!(
                "  mock  {} {}",
                ffmpeg_bin().display(),
                spec.args().join(" ")
            );
        }
        for d in dests {
            let spec = PusherSpec::from_config(cfg, d.url, &ass);
            println!(
                "  {:<8} {} {}",
                d.name,
                ffmpeg_bin().display(),
                spec.args().join(" ")
            );
        }
    }

    println!("\nready.");
    Ok(())
}
