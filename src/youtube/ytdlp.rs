use std::io::ErrorKind;
use std::process::Output;
use std::time::Duration;

use tokio::process::Command;

pub const STDERR_TRUNCATE_LEN: usize = 200;

const DEPENDENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

const VERSION_LOG_LEN: usize = 120;

/// Keeps the last `max_len` characters of `s`, prefixed with `...` when it
/// was cut. yt-dlp's most useful line is usually the last one, so trimming
/// the head keeps the actual error instead of leading warnings.
pub fn truncate_tail(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        return s.to_string();
    }
    let byte_idx = s
        .char_indices()
        .nth(char_count - max_len)
        .map_or(0, |(idx, _)| idx);
    format!("...{}", &s[byte_idx..])
}

#[derive(Debug, thiserror::Error)]
pub enum YtDlpError {
    #[error("yt-dlp is not installed or not on PATH")]
    Missing,
    #[error("yt-dlp timed out")]
    Timeout,
    #[error("failed to run yt-dlp: {0}")]
    Spawn(String),
    #[error("yt-dlp failed: {0}")]
    Failed(String),
}

/// Thin wrapper around the `yt-dlp` subprocess: cookie injection, timeout,
/// `kill_on_drop` and stderr capture in one place.
#[derive(Debug, Clone, Default)]
pub struct YtDlp {
    cookies_file: Option<String>,
}

impl YtDlp {
    pub fn new(cookies_file: Option<String>) -> Self {
        Self { cookies_file }
    }

    async fn spawn(&self, args: &[&str], timeout: Duration) -> Result<Output, YtDlpError> {
        let mut command = Command::new("yt-dlp");
        command.kill_on_drop(true);
        command.args(args);
        if let Some(cookies_file) = &self.cookies_file {
            command.args(["--cookies", cookies_file]);
        }

        tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_elapsed| YtDlpError::Timeout)?
            .map_err(|e| {
                if e.kind() == ErrorKind::NotFound {
                    YtDlpError::Missing
                } else {
                    YtDlpError::Spawn(e.to_string())
                }
            })
    }

    /// Runs yt-dlp with `args`, failing on a non-zero exit status.
    pub async fn run(&self, args: &[&str], timeout: Duration) -> Result<String, YtDlpError> {
        let output = self.spawn(args, timeout).await?;
        if !output.status.success() {
            return Err(YtDlpError::Failed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs yt-dlp with `args` and returns stdout regardless of exit status.
    /// For callers that pass `--ignore-errors` and parse whatever came back
    /// rather than treating a non-zero exit as a hard failure.
    pub async fn run_ignoring_status(
        &self,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String, YtDlpError> {
        let output = self.spawn(args, timeout).await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Checks that the `yt-dlp` binary is present and runnable.
    pub async fn probe() -> Result<(), YtDlpError> {
        if binary_runnable("yt-dlp", "--version").await {
            Ok(())
        } else {
            Err(YtDlpError::Missing)
        }
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

    #[test]
    fn truncate_tail_keeps_final_error_over_leading_warning() {
        let stderr = format!(
            "WARNING: [youtube] {}\nERROR: [youtube] xyz: Requested format is not available.\n",
            "x".repeat(300)
        );
        let truncated = truncate_tail(stderr.trim_end(), STDERR_TRUNCATE_LEN);
        assert!(truncated.contains("Requested format is not available"));
    }

    #[test]
    fn truncate_tail_leaves_short_strings_untouched() {
        assert_eq!(truncate_tail("short", 200), "short");
    }
}
