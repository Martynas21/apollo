pub mod ipc_backend;
pub mod panel;
pub mod player;
pub mod radio;
pub mod resolve;

pub use player::{PlayerRegistry, QueuedTrack};

use std::time::Duration;

use tokio::process::Command;

const DEPENDENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

const VERSION_LOG_LEN: usize = 120;

#[allow(dead_code)]
pub async fn check_playback_dependencies() -> anyhow::Result<()> {
    if binary_runnable("yt-dlp", "--version").await {
        Ok(())
    } else {
        anyhow::bail!("yt-dlp is missing — install it and ensure it's on PATH")
    }
}

fn version_banner(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(VERSION_LOG_LEN) {
        Some((byte_idx, _)) => format!("{}...", &line[..byte_idx]),
        None => line.to_string(),
    }
}

async fn binary_runnable(program: &str, version_arg: &str) -> bool {
    binary_runnable_within(program, version_arg, DEPENDENCY_PROBE_TIMEOUT).await
}

async fn binary_runnable_within(program: &str, version_arg: &str, timeout: Duration) -> bool {
    let probe = Command::new(program)
        .arg(version_arg)
        .kill_on_drop(true)
        .output();

    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(output)) if output.status.success() => {
            tracing::info!(
                program,
                version = version_banner(&String::from_utf8_lossy(&output.stdout)),
                "playback dependency ok"
            );
            true
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                program,
                status = %output.status,
                stderr = version_banner(&String::from_utf8_lossy(&output.stderr)),
                "playback dependency exited non-zero"
            );
            false
        }
        Ok(Err(err)) => {
            tracing::warn!(program, %err, "playback dependency could not be run");
            false
        }
        Err(_) => {
            tracing::warn!(
                program,
                timeout_secs = timeout.as_secs(),
                "playback dependency check timed out"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binary_runnable_reports_false_for_nonexistent_program() {
        assert!(!binary_runnable("apollo-test-definitely-not-a-real-binary", "--version").await);
    }

    #[tokio::test]
    async fn binary_runnable_reports_false_for_a_hanging_program() {
        let hung = binary_runnable_within("sleep", "30", Duration::from_millis(100)).await;
        assert!(!hung);
    }

    #[tokio::test]
    async fn binary_runnable_reports_false_for_a_nonzero_exit() {
        assert!(!binary_runnable_within("false", "", Duration::from_secs(5)).await);
    }

    #[test]
    fn version_banner_keeps_only_a_truncated_first_line() {
        let banner = version_banner("yt-dlp 2024.08.06\nsome other line\n");
        assert_eq!(banner, "yt-dlp 2024.08.06");

        let long = format!("yt-dlp version {}\nconfiguration: ...", "x".repeat(300));
        let banner = version_banner(&long);
        assert!(banner.ends_with("..."));
        assert!(banner.chars().count() <= VERSION_LOG_LEN + 3);
    }
}
