//! Small process and path helpers.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Run a command and return its stdout ("" on any failure or after `timeout`).
pub fn sh(args: &[&str], timeout: Duration) -> String {
    let Some((cmd, rest)) = args.split_first() else { return String::new() };
    let Ok(mut child) = Command::new(cmd).args(rest).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    else {
        return String::new();
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return String::new();
            }
        }
    }
    child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// Exit status of a command, quietly (false if it couldn't run).
pub fn ok(args: &[&str]) -> bool {
    let Some((cmd, rest)) = args.split_first() else { return false };
    Command::new(cmd)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
