//! Live smoke: publish testsrc into a temp daemon and pull it back.
//! Requires ffmpeg on PATH.

use std::net::TcpListener;
use std::process::Stdio;
use std::time::Duration;

use echochamber::config::{write_init_template, Config};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn relay_roundtrip() {
    if !ffmpeg() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_init_template(&cfg_path).unwrap();
    let mut text = std::fs::read_to_string(&cfg_path).unwrap();
    let rtmp_port = free_port();
    let http_port = free_port();
    text = text.replace("127.0.0.1:1935", &format!("127.0.0.1:{rtmp_port}"));
    text = text.replace("127.0.0.1:8080", &format!("127.0.0.1:{http_port}"));
    std::fs::write(&cfg_path, text).unwrap();
    let cfg = Config::load_from_path(&cfg_path).unwrap();

    let serve = tokio::spawn(echochamber::daemon::serve(cfg, cfg_path.clone()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let url = format!("rtmp://127.0.0.1:{rtmp_port}/echochamber/live");
    let out = dir.path().join("out.flv");

    let mut pubr = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-t",
            "4",
            "-shortest",
            "-f",
            "flv",
            &url,
        ])
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    let pull = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-rw_timeout",
            "8000000",
            "-i",
            &url,
            "-t",
            "2",
            "-c",
            "copy",
            "-y",
            out.to_str().unwrap(),
        ])
        .output();

    let pulled = tokio::time::timeout(Duration::from_secs(20), pull)
        .await
        .expect("pull timed out")
        .expect("pull spawn");

    let _ = pubr.start_kill();
    let _ = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/stop"))
        .send()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;

    assert!(
        pulled.status.success(),
        "ffmpeg pull failed: {}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let meta = std::fs::metadata(&out).expect("output file");
    assert!(meta.len() > 1000, "pulled flv too small: {}", meta.len());
}

/// OBS publishes as app=`echochamber` + stream key=`live`. ffmpeg must play
/// with -rtmp_app / -rtmp_playpath, not `.../echochamber/live` as the app name.
#[tokio::test]
async fn obs_style_publish_then_pusher_play() {
    if !ffmpeg() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_init_template(&cfg_path).unwrap();
    let mut text = std::fs::read_to_string(&cfg_path).unwrap();
    let rtmp_port = free_port();
    let http_port = free_port();
    text = text.replace("127.0.0.1:1935", &format!("127.0.0.1:{rtmp_port}"));
    text = text.replace("127.0.0.1:8080", &format!("127.0.0.1:{http_port}"));
    std::fs::write(&cfg_path, text).unwrap();
    let cfg = Config::load_from_path(&cfg_path).unwrap();

    let serve = tokio::spawn(echochamber::daemon::serve(cfg.clone(), cfg_path.clone()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let tc = format!("rtmp://127.0.0.1:{rtmp_port}/echochamber");
    let out = dir.path().join("out.flv");

    let mut pubr = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-t",
            "5",
            "-shortest",
            "-rtmp_app",
            "echochamber",
            "-rtmp_playpath",
            "live",
            "-f",
            "flv",
            &tc,
        ])
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(900)).await;

    let spec = echochamber::ffmpeg::PusherSpec::from_config(
        &cfg,
        out.to_str().unwrap().to_string(),
        std::path::Path::new("echochamber_live.ass"),
    );
    // Drop reconnect/progress-only concerns: file output. Still uses rtmp_app/playpath on input.
    let pull_args = spec.args();

    let pulled = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("ffmpeg")
            .args(pull_args)
            .output(),
    )
    .await
    .expect("pull timed out")
    .expect("pull spawn");

    let _ = pubr.start_kill();
    let _ = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/stop"))
        .send()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;

    assert!(
        pulled.status.success(),
        "OBS-style play failed: {}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let meta = std::fs::metadata(&out).expect("output file");
    assert!(meta.len() > 1000, "pulled flv too small: {}", meta.len());
}

/// OBS with Server=`rtmp://host/echochamber/live` (stream name in the URL)
/// publishes as app=`echochamber/live` + key=`live`. Pushers must play that app.
#[tokio::test]
async fn obs_url_includes_stream_name() {
    if !ffmpeg() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_init_template(&cfg_path).unwrap();
    let mut text = std::fs::read_to_string(&cfg_path).unwrap();
    let rtmp_port = free_port();
    let http_port = free_port();
    text = text.replace("127.0.0.1:1935", &format!("127.0.0.1:{rtmp_port}"));
    text = text.replace("127.0.0.1:8080", &format!("127.0.0.1:{http_port}"));
    std::fs::write(&cfg_path, text).unwrap();
    let cfg = Config::load_from_path(&cfg_path).unwrap();

    let serve = tokio::spawn(echochamber::daemon::serve(cfg.clone(), cfg_path.clone()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let url = format!("rtmp://127.0.0.1:{rtmp_port}/echochamber/live");
    let out = dir.path().join("out.flv");

    let mut pubr = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-t",
            "5",
            "-shortest",
            "-rtmp_app",
            "echochamber/live",
            "-rtmp_playpath",
            "live",
            "-f",
            "flv",
            &url,
        ])
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(900)).await;

    let published = echochamber::state::PublishedStream {
        app: "echochamber/live".into(),
        stream_key: "live".into(),
    };
    let spec = echochamber::ffmpeg::PusherSpec::from_published(
        &cfg,
        out.to_str().unwrap().to_string(),
        std::path::Path::new("echochamber_live.ass"),
        Some(&published),
    );
    let pulled = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("ffmpeg")
            .args(spec.args())
            .output(),
    )
    .await
    .expect("pull timed out")
    .expect("pull spawn");

    let _ = pubr.start_kill();
    let _ = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/stop"))
        .send()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;

    assert!(
        pulled.status.success(),
        "play of echochamber/live failed: {}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let meta = std::fs::metadata(&out).expect("output file");
    assert!(meta.len() > 1000, "pulled flv too small: {}", meta.len());
}

/// OBS stop/start is a new publish session. Ingest must still be playable
/// (GOP from the first session must not poison the second).
#[tokio::test]
async fn stop_then_resume_still_plays() {
    if !ffmpeg() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_init_template(&cfg_path).unwrap();
    let mut text = std::fs::read_to_string(&cfg_path).unwrap();
    let rtmp_port = free_port();
    let http_port = free_port();
    text = text.replace("127.0.0.1:1935", &format!("127.0.0.1:{rtmp_port}"));
    text = text.replace("127.0.0.1:8080", &format!("127.0.0.1:{http_port}"));
    std::fs::write(&cfg_path, text).unwrap();
    let cfg = Config::load_from_path(&cfg_path).unwrap();

    let serve = tokio::spawn(echochamber::daemon::serve(cfg, cfg_path.clone()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let tc = format!("rtmp://127.0.0.1:{rtmp_port}/echochamber");
    let publish_args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=1000:sample_rate=44100",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "25",
        "-c:a",
        "aac",
        "-t",
        "2",
        "-shortest",
        "-rtmp_app",
        "echochamber",
        "-rtmp_playpath",
        "live",
        "-f",
        "flv",
        tc.as_str(),
    ];

    let mut first = tokio::process::Command::new("ffmpeg")
        .args(publish_args)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(8), first.wait()).await;
    let _ = first.start_kill();

    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut second = tokio::process::Command::new("ffmpeg")
        .args(publish_args)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let out = dir.path().join("resume.flv");
    let pulled = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-rw_timeout",
                "8000000",
                "-rtmp_live",
                "live",
                "-rtmp_app",
                "echochamber",
                "-rtmp_playpath",
                "live",
                "-i",
                &format!("rtmp://127.0.0.1:{rtmp_port}"),
                "-t",
                "1",
                "-c",
                "copy",
                "-y",
                out.to_str().unwrap(),
            ])
            .output(),
    )
    .await
    .expect("resume pull timed out")
    .expect("resume pull spawn");

    let _ = second.start_kill();
    let _ = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/stop"))
        .send()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;

    assert!(
        pulled.status.success(),
        "play after OBS resume failed: {}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let meta = std::fs::metadata(&out).expect("resume output file");
    assert!(meta.len() > 1000, "resume flv too small: {}", meta.len());
}

/// With dests configured and OBS down, the slate must produce bytes.
#[tokio::test]
async fn standby_loops_to_file_dest_without_publisher() {
    if !ffmpeg() {
        eprintln!("skip: ffmpeg not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("slate.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=220:sample_rate=48000",
            "-t",
            "1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-y",
            clip.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(made.success(), "failed to make slate fixture");

    let cfg_path = dir.path().join("config.toml");
    write_init_template(&cfg_path).unwrap();
    let mut text = std::fs::read_to_string(&cfg_path).unwrap();
    let rtmp_port = free_port();
    let http_port = free_port();
    text = text.replace("127.0.0.1:1935", &format!("127.0.0.1:{rtmp_port}"));
    text = text.replace("127.0.0.1:8080", &format!("127.0.0.1:{http_port}"));
    text = text.replace(
        "enabled = false\nafter_first_publish = true",
        "enabled = true\nafter_first_publish = false",
    );
    text = text.replace(
        "video = \"assets/standby.mp4\"",
        &format!("video = \"{}\"", clip.display()),
    );
    text = text.replace("audio = \"assets/standby.m4a\"", "audio = \"\"");
    text = text.replace("width = 1920", "width = 320");
    text = text.replace("height = 1080", "height = 240");
    text = text.replace("fps = 30", "fps = 15");
    text = text.replace("delay_secs = 2", "delay_secs = 0");
    std::fs::write(&cfg_path, text).unwrap();
    let cfg = Config::load_from_path(&cfg_path).unwrap();

    let serve = tokio::spawn(echochamber::daemon::serve(cfg, cfg_path.clone()));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = dir.path().join("slate_out.flv");
    let client = reqwest::Client::new();
    let added = client
        .post(format!("http://127.0.0.1:{http_port}/push"))
        .json(&serde_json::json!({ "url": out.to_str().unwrap(), "name": "file" }))
        .send()
        .await
        .unwrap();
    assert!(added.status().is_success(), "push dest failed");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut n = 0u64;
    let mut last_status = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{http_port}/status"))
            .send()
            .await
        {
            last_status = resp.text().await.unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&last_status) {
                n = v["pushers"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|p| p["bytes_out"].as_u64())
                    .unwrap_or(0);
                if n > 2000 {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let _ = client
        .post(format!("http://127.0.0.1:{http_port}/stop"))
        .send()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;

    assert!(
        n > 2000,
        "standby dest flv too small: {n}\nstatus={last_status}\nvideo={}",
        clip.display()
    );
}
