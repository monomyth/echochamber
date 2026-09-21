use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::SubtitleStyle;

#[derive(Debug, Clone)]
pub struct Cue {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

#[derive(Debug)]
pub struct SubtitleClock {
    t0: Option<Duration>,
    last_end: Duration,
}

impl Default for SubtitleClock {
    fn default() -> Self {
        Self {
            t0: None,
            last_end: Duration::ZERO,
        }
    }
}

impl SubtitleClock {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Anchor the first observed media timestamp as t0 and return timeline times.
    pub fn map(&mut self, start: Duration, end: Duration) -> Option<(Duration, Duration)> {
        if end <= start {
            return None;
        }
        let t0 = *self.t0.get_or_insert(start);
        let start = start.saturating_sub(t0);
        let end = end.saturating_sub(t0);
        if start < self.last_end {
            tracing::debug!(
                start_ms = start.as_millis(),
                last_end_ms = self.last_end.as_millis(),
                "skipping non-monotonic subtitle cue"
            );
            return None;
        }
        self.last_end = end;
        Some((start, end))
    }
}

pub struct SrtWriter {
    path: PathBuf,
    file: BufWriter<File>,
    index: u32,
}

impl SrtWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path).with_context(|| format!("create srt {}", path.display()))?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
            index: 1,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_cue(&mut self, cue: &Cue) -> Result<()> {
        write!(
            self.file,
            "{}\n{} --> {}\n{}\n\n",
            self.index,
            srt_ts(cue.start),
            srt_ts(cue.end),
            cue.text
        )?;
        self.file.flush()?;
        self.index += 1;
        Ok(())
    }
}

pub struct AssWriter {
    path: PathBuf,
    file: BufWriter<File>,
}

impl AssWriter {
    pub fn create(path: impl AsRef<Path>, style: SubtitleStyle) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path).with_context(|| format!("create ass {}", path.display()))?;
        let mut w = BufWriter::new(file);
        w.write_all(generate_ass_header(style).as_bytes())?;
        w.flush()?;
        Ok(Self { path, file: w })
    }

    pub fn open_append(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_cue(&mut self, cue: &Cue) -> Result<()> {
        let line = format!(
            "Dialogue: 0,{},{},Default,,0,0,0,,{}\n",
            ass_ts(cue.start),
            ass_ts(cue.end),
            cue.text.replace('\n', "\\N")
        );
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}

pub fn generate_ass_header(style: SubtitleStyle) -> String {
    let (name, font, size, primary, outline, margin_v) = match style {
        SubtitleStyle::Modern => ("Default", "Arial", 48, "&H00FFFFFF", 2, 40),
        SubtitleStyle::Minimal => ("Default", "Arial", 32, "&H00FFFFFF", 1, 24),
        SubtitleStyle::Broadcast => ("Default", "Arial", 52, "&H0000FFFF", 3, 60),
    };
    format!(
        "\
[Script Info]
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080
WrapStyle: 0
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: {name},{font},{size},{primary},&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,{outline},0,2,40,40,{margin_v},1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"
    )
}

pub fn srt_ts(d: Duration) -> String {
    let ms_total = d.as_millis();
    let hours = ms_total / 3_600_000;
    let minutes = (ms_total % 3_600_000) / 60_000;
    let seconds = (ms_total % 60_000) / 1_000;
    let millis = ms_total % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

pub fn ass_ts(d: Duration) -> String {
    let cs_total = d.as_millis() / 10;
    let hours = cs_total / 360_000;
    let minutes = (cs_total % 360_000) / 6_000;
    let seconds = (cs_total % 6_000) / 100;
    let cs = cs_total % 100;
    format!("{hours}:{minutes:02}:{seconds:02}.{cs:02}")
}

/// Parse a simple SRT produced by whisper-cli `-osrt`.
pub fn parse_srt(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        // optional index
        let timing = if line.contains("-->") {
            line
        } else {
            match lines.next() {
                Some(l) if l.contains("-->") => l,
                _ => continue,
            }
        };
        let Some((start_s, end_s)) = timing.split_once("-->") else {
            continue;
        };
        let mut text_lines = Vec::new();
        while let Some(peek) = lines.peek() {
            if peek.trim().is_empty() {
                lines.next();
                break;
            }
            if peek.contains("-->") {
                break;
            }
            text_lines.push(lines.next().unwrap().to_string());
        }
        if let (Some(start), Some(end)) = (parse_srt_ts(start_s.trim()), parse_srt_ts(end_s.trim()))
        {
            let text = text_lines.join(" ").trim().to_string();
            if !text.is_empty() {
                cues.push(Cue { start, end, text });
            }
        }
    }
    cues
}

fn parse_srt_ts(s: &str) -> Option<Duration> {
    // HH:MM:SS,mmm
    let s = s.replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let sec_parts: Vec<&str> = parts[2].split('.').collect();
    let sec: u64 = sec_parts[0].parse().ok()?;
    let millis: u64 = sec_parts
        .get(1)
        .map(|p| {
            let mut p = p.to_string();
            while p.len() < 3 {
                p.push('0');
            }
            p[..3].parse().unwrap_or(0)
        })
        .unwrap_or(0);
    Some(Duration::from_millis(
        h * 3_600_000 + m * 60_000 + sec * 1_000 + millis,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn timestamps_format() {
        let d = Duration::from_millis(3_661_230);
        assert_eq!(srt_ts(d), "01:01:01,230");
        assert_eq!(ass_ts(d), "1:01:01.23");
    }

    #[test]
    fn clock_skips_non_monotonic() {
        let mut c = SubtitleClock::default();
        let a = c
            .map(Duration::from_secs(8), Duration::from_secs(10))
            .unwrap();
        assert_eq!(a.0, Duration::ZERO);
        assert!(c
            .map(Duration::from_secs(8), Duration::from_secs(9))
            .is_none());
        let b = c
            .map(Duration::from_secs(11), Duration::from_secs(13))
            .unwrap();
        assert_eq!(b.0, Duration::from_secs(3));
    }

    #[test]
    fn ass_and_srt_roundtrip_files() {
        let dir = tempdir().unwrap();
        let mut srt = SrtWriter::create(dir.path().join("t.srt")).unwrap();
        let mut ass = AssWriter::create(dir.path().join("t.ass"), SubtitleStyle::Modern).unwrap();
        let cue = Cue {
            start: Duration::from_millis(1230),
            end: Duration::from_millis(2500),
            text: "hello world".into(),
        };
        srt.write_cue(&cue).unwrap();
        ass.write_cue(&cue).unwrap();
        let srt_text = std::fs::read_to_string(srt.path()).unwrap();
        assert!(srt_text.contains("00:00:01,230 --> 00:00:02,500"));
        let ass_text = std::fs::read_to_string(ass.path()).unwrap();
        assert!(ass_text.contains("[V4+ Styles]"));
        assert!(ass_text.contains("Dialogue: 0,0:00:01.23,0:00:02.50,Default,,0,0,0,,hello world"));
    }

    #[test]
    fn parse_whisper_srt() {
        let text = "\
1
00:00:00,000 --> 00:00:01,500
hello

2
00:00:01,500 --> 00:00:02,000
world
";
        let cues = parse_srt(text);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "hello");
        assert_eq!(cues[1].end, Duration::from_secs(2));
    }
}
