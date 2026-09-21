/// Mask a secret so only the last 4 characters remain visible.
/// Keys containing `/` or `?` are treated as a single token (never sliced on those).
pub fn redact_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let keep = 4usize;
    let mut out = String::from("***");
    out.extend(chars[chars.len() - keep..].iter());
    out
}

/// ffmpeg argv with the destination RTMP URL (last arg) masked.
pub fn redact_cmd(bin: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(bin.to_string());
    for a in args {
        if should_redact_rtmp(a) {
            parts.push(redact_embedded_rtmp(a));
        } else {
            parts.push(a.clone());
        }
    }
    parts.join(" ")
}

fn should_redact_rtmp(s: &str) -> bool {
    // A publish URL has a path after the host. `rtmp://127.0.0.1:1935` does not.
    s.find("://")
        .is_some_and(|i| s[i + 3..].contains('/'))
}

/// Mask stream keys inside a tee spec (`rtmp\://host/app/key`).
fn redact_embedded_rtmp(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &s[i..];
        let hit = rest.find("rtmp://").or_else(|| rest.find(r"rtmp\://"));
        let Some(rel) = hit else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..rel]);
        let start = i + rel;
        let mut end = start;
        let chars: Vec<(usize, char)> = s[start..].char_indices().collect();
        let mut k = 0;
        while k < chars.len() {
            let (off, c) = chars[k];
            if c == '\\' {
                end = start + off + c.len_utf8();
                k += 1;
                if k < chars.len() {
                    end = start + chars[k].0 + chars[k].1.len_utf8();
                    k += 1;
                }
                continue;
            }
            if c == '|' || c == ' ' || c == '"' || c == '\'' {
                break;
            }
            end = start + off + c.len_utf8();
            k += 1;
        }
        let url = s[start..end].replace(r"\:", ":");
        let masked = redact_url(&url).replace("://", r"\://");
        // Keep original escaping style if the source was escaped.
        if s[start..end].contains(r"\:") {
            out.push_str(&masked);
        } else {
            out.push_str(&redact_url(&url));
        }
        i = end;
    }
    out
}

fn is_rtmp_like(s: &str) -> bool {
    let u = s.to_ascii_lowercase();
    u.starts_with("rtmp://") || u.starts_with("rtmps://")
}

pub fn redact_url(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    if !is_rtmp_like(url) {
        return url.to_string();
    }
    // RTMP publish URLs typically end with the stream key after the last '/'.
    match url.rfind('/') {
        Some(idx) if idx + 1 < url.len() => {
            format!("{}{}", &url[..=idx], redact_key(&url[idx + 1..]))
        }
        _ => redact_key(url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_keys_are_fully_masked() {
        assert_eq!(redact_key("ab"), "**");
        assert_eq!(redact_key("abcd"), "****");
    }

    #[test]
    fn long_keys_keep_last_four() {
        assert_eq!(redact_key("youtube-stream-key-xyz9"), "***xyz9");
    }

    #[test]
    fn slash_and_query_are_not_special() {
        assert_eq!(redact_key("abc/def?ghi"), "***?ghi");
    }

    #[test]
    fn redact_url_masks_final_segment() {
        let url = "rtmp://a.rtmp.youtube.com/live2/super-secret-key";
        let masked = redact_url(url);
        assert!(masked.starts_with("rtmp://a.rtmp.youtube.com/live2/"));
        assert!(!masked.contains("super-secret"));
        assert!(masked.ends_with("key"));
    }

    #[test]
    fn redact_cmd_masks_output_key_only() {
        let args = [
            "-i",
            "rtmp://127.0.0.1:1935",
            "-f",
            "flv",
            "rtmp://a.rtmp.youtube.com/live2/super-secret-key",
        ]
        .map(String::from);
        let cmd = redact_cmd("ffmpeg", &args);
        assert!(cmd.contains("rtmp://127.0.0.1:1935"));
        assert!(!cmd.contains("super-secret"));
        assert!(cmd.contains("rtmp://a.rtmp.youtube.com/live2/"));
    }

    #[test]
    fn redact_cmd_masks_tee_keys() {
        let spec = r"[f=flv:onfail=ignore]rtmp\://a.example/live/super-secret|[f=flv]rtmp\://b.example/app/other-secret".to_string();
        let cmd = redact_cmd("ffmpeg", &[spec]);
        assert!(!cmd.contains("super-secret"));
        assert!(!cmd.contains("other-secret"));
    }
}
