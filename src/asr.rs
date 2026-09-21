use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::ffmpeg::{asr_extract_args, ffmpeg_bin, whisper_bin};
use crate::process::{spawn_grouped, terminate};
use crate::pusher::sidecar_paths;
use crate::state::{bump_drops, Shared};
use crate::subtitles::{parse_srt, AssWriter, Cue, SrtWriter, SubtitleClock};

const MIN_WAV_BYTES: u64 = 4000;

pub async fn run(shared: Shared) {
    let (srt_path, ass_path) = sidecar_paths();
    let mut srt = None;
    let mut ass = None;
    let mut clock = SubtitleClock::default();
    let mut chunk_index: u64 = 0;
    let mut publishing = shared.publishing.clone();
    let tmp_dir = std::env::temp_dir();

    loop {
        tokio::select! {
            _ = shared.shutdown.notified() => break,
            _ = publishing.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(250)), if *publishing.borrow() => {}
        }

        let cfg = shared.config.borrow().clone();
        {
            let mut st = shared.asr.lock().await;
            st.enabled = cfg.subtitles.enabled;
        }
        if !cfg.subtitles.enabled || !*publishing.borrow() {
            continue;
        }

        if srt.is_none() && cfg.subtitles.sidecar {
            match SrtWriter::create(&srt_path) {
                Ok(w) => srt = Some(w),
                Err(e) => warn!(error = %e, "srt writer"),
            }
        }
        if ass.is_none() && (cfg.subtitles.burn_in || cfg.subtitles.sidecar) {
            match AssWriter::create(&ass_path, cfg.subtitles.style) {
                Ok(w) => ass = Some(w),
                Err(e) => warn!(error = %e, "ass writer"),
            }
        }

        let wav = tmp_dir.join(format!("echo_asr_{}.wav", std::process::id()));
        let _ = std::fs::remove_file(&wav);
        let published = shared.published.lock().await.clone();
        let args = asr_extract_args(&cfg, published.as_ref(), &wav, cfg.subtitles.chunk_seconds);
        debug!("asr extract {} {}", ffmpeg_bin().display(), args.join(" "));

        match spawn_grouped(&ffmpeg_bin(), &args) {
            Ok(mut child) => {
                let status = tokio::select! {
                    _ = shared.shutdown.notified() => {
                        terminate(&mut child).await;
                        break;
                    }
                    s = child.wait() => s,
                };
                match status {
                    Ok(st) => debug!(%st, "asr ffmpeg finished"),
                    Err(e) => warn!(error = %e, "asr ffmpeg wait"),
                }
            }
            Err(e) => {
                warn!(error = %e, "asr ffmpeg spawn");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        }

        let ok = match std::fs::metadata(&wav) {
            Ok(m) if m.len() > MIN_WAV_BYTES => true,
            Ok(m) => {
                warn!(bytes = m.len(), "asr wav too small, retrying");
                let _ = std::fs::remove_file(&wav);
                bump_drops(&shared);
                tokio::time::sleep(Duration::from_millis(400)).await;
                false
            }
            Err(_) => {
                warn!("asr wav missing after extract, retrying");
                bump_drops(&shared);
                tokio::time::sleep(Duration::from_millis(400)).await;
                false
            }
        };
        if !ok {
            continue;
        }

        match transcribe(&cfg, &wav).await {
            Ok(cues) => {
                let offset = Duration::from_secs(chunk_index * cfg.subtitles.chunk_seconds);
                chunk_index += 1;
                for mut cue in cues {
                    cue.start += offset;
                    cue.end += offset;
                    if let Some((s, e)) = clock.map(cue.start, cue.end) {
                        cue.start = s;
                        cue.end = e;
                        if let Some(w) = srt.as_mut() {
                            let _ = w.write_cue(&cue);
                        }
                        if let Some(w) = ass.as_mut() {
                            let _ = w.write_cue(&cue);
                        }
                        let mut st = shared.asr.lock().await;
                        st.cues += 1;
                        st.last_text = Some(cue.text.clone());
                        st.last_error = None;
                        info!(text = %cue.text, "asr cue");
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "whisper");
                shared.asr.lock().await.last_error = Some(e.to_string());
            }
        }
        let _ = std::fs::remove_file(&wav);
    }
}

async fn transcribe(cfg: &Config, wav: &PathBuf) -> anyhow::Result<Vec<Cue>> {
    if cfg.subtitles.whisper_model.trim().is_empty() {
        anyhow::bail!("subtitles.whisper_model is empty");
    }
    let out_base = wav.with_extension("");
    let mut cmd = Command::new(whisper_bin());
    cmd.arg("-m")
        .arg(&cfg.subtitles.whisper_model)
        .arg("-f")
        .arg(wav)
        .arg("-l")
        .arg(whisper_lang(&cfg.subtitles.language))
        .arg("-nt")
        .arg("-np")
        .arg("-osrt")
        .arg("-of")
        .arg(&out_base)
        .kill_on_drop(true);
    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "whisper-cli failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let srt_path = PathBuf::from(format!("{}.srt", out_base.display()));
    let text = std::fs::read_to_string(&srt_path).unwrap_or_default();
    let _ = std::fs::remove_file(&srt_path);
    Ok(parse_srt(&text))
}

fn whisper_lang(lang: &str) -> &str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "en" | "english" => "en",
        "ru" | "russian" => "ru",
        _ => "auto",
    }
}
