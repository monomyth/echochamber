use std::io;
use std::process::Stdio;

use tokio::process::{Child, Command};

/// Spawn ffmpeg/whisper in its own process group so shutdown can SIGTERM the tree.
pub fn spawn_grouped(bin: &std::path::Path, args: &[String]) -> io::Result<Child> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd.spawn()
}

pub fn kill_group(pid: u32, sig: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), sig);
        libc::kill(pid as i32, sig);
    }
    #[cfg(not(unix))]
    let _ = (pid, sig);
}

pub async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_group(pid, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(std::time::Duration::from_millis(800), child.wait()).await;
    if let Some(pid) = child.id() {
        kill_group(pid, libc::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
