//! Fire-and-forget status push to NERV (`POST /api/status/report`).
//!
//! Agreed design (Magi + Vela, 2026-08-27): NERV stores metadata-only status in its
//! existing `status_cache` table, keyed by identity. Identity is established by the
//! bearer token server-side, so no identity field travels in the request body — only
//! [`Config::nerv_base_url`](crate::config::Config::nerv_base_url) and
//! [`nerv_bearer_token`](crate::config::Config::nerv_bearer_token) are needed here.
//!
//! This module only builds the payload and fires the request. The caller ([`crate::live_status`])
//! owns *when*: on every state transition, plus a periodic heartbeat so NERV can tell "the
//! bridge is running but idle" from "the bridge has gone silent". Reporting is entirely
//! optional — a bridge with neither `NERV_BASE_URL` nor `NERV_BEARER_TOKEN` set behaves
//! exactly as it did before this existed.
//!
//! Never blocks Matrix status posting: every call spawns its own task and races it against
//! [`REPORT_TIMEOUT`], so a slow or unreachable NERV can cost at most that much of a
//! background task's time, never the tick loop's.

use std::time::Duration;

use serde::Serialize;

use crate::status::{AgentState, AgentStatus};

/// How long a single push may run before being abandoned.
const REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Heartbeat cadence: sent even when nothing changed, so silence itself is a signal.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// NERV connection details. Constructed once at startup; `None` when reporting isn't
/// configured.
#[derive(Clone)]
pub struct NervTarget {
    base_url: String,
    bearer_token: String,
}

impl NervTarget {
    /// Builds a target from [`Config`](crate::config::Config), or `None` if either half of
    /// the pair is missing — deliberately not an error, since NERV reporting is optional.
    pub fn from_config(config: &crate::config::Config) -> Option<Self> {
        let base_url = config.nerv_base_url.clone()?;
        let bearer_token = config.nerv_bearer_token.clone()?;
        Some(Self {
            base_url,
            bearer_token,
        })
    }

    fn report_url(&self) -> String {
        format!("{}/api/status/report", self.base_url.trim_end_matches('/'))
    }
}

/// Metadata-only payload for `POST /api/status/report` — no transcript content, no room
/// or message identifiers, no identity field (the bearer token carries that). Mirrors the
/// privacy note in [`crate::status`]: this is exactly the same shape of information the
/// live Matrix status message already renders, sent somewhere else.
#[derive(Serialize, Debug, PartialEq)]
pub struct StatusReport {
    pub state: &'static str,
    pub last_activity_age_secs: Option<u64>,
    pub last_tool: Option<String>,
    pub turn_elapsed_secs: Option<u64>,
    pub last_reply_age_secs: Option<u64>,
    /// When this report was built, RFC3339 UTC — independent of NERV's own receipt time, so
    /// staleness can still be measured if a report gets queued or retried somewhere in transit.
    pub updated_at: String,
}

impl StatusReport {
    /// `now` is a parameter rather than an internal `SystemTime::now()` call so this stays a
    /// pure, deterministically testable function — see [`crate::status::read_status_at`] for
    /// the same `_at`-suffix-free-of-the-clock convention elsewhere in this codebase.
    pub fn from_status(status: &AgentStatus, now: std::time::SystemTime) -> Self {
        Self {
            state: state_label(status.state),
            last_activity_age_secs: status.last_activity_age.map(|d| d.as_secs()),
            last_tool: status.last_tool.clone(),
            turn_elapsed_secs: status.turn_elapsed.map(|d| d.as_secs()),
            last_reply_age_secs: status.last_reply_age.map(|d| d.as_secs()),
            updated_at: crate::matrix::humanize_timestamp(now),
        }
    }
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::WaitingForUser => "waiting_for_user",
        AgentState::Stalled => "stalled",
        AgentState::Dead => "dead",
        AgentState::Unknown => "unknown",
    }
}

/// Fires the status report at NERV without waiting for it or letting it affect the
/// caller. Failures — network error, timeout, non-2xx response — are logged at `debug`
/// only: visible when diagnosing, silent otherwise, since a NERV hiccup is not a
/// Matrix-bridge problem and must never read as one.
pub fn spawn_report(http: reqwest::Client, target: NervTarget, report: StatusReport) {
    tokio::spawn(async move {
        let url = target.report_url();
        let attempt = http
            .post(&url)
            .bearer_auth(&target.bearer_token)
            .json(&report)
            .send();

        match tokio::time::timeout(REPORT_TIMEOUT, attempt).await {
            Ok(Ok(resp)) if resp.status().is_success() => {
                tracing::debug!(%url, status = %resp.status(), "NERV status report accepted");
            }
            Ok(Ok(resp)) => {
                tracing::debug!(%url, status = %resp.status(), "NERV status report rejected");
            }
            Ok(Err(error)) => {
                tracing::debug!(%url, %error, "NERV status report failed");
            }
            Err(_) => {
                tracing::debug!(
                    %url,
                    timeout_secs = REPORT_TIMEOUT.as_secs(),
                    "NERV status report timed out"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use clap::Parser;

    fn base_config() -> Config {
        Config::parse_from(["cc_matrix_channel"])
    }

    #[test]
    fn from_config_none_when_unconfigured() {
        assert!(NervTarget::from_config(&base_config()).is_none());
    }

    #[test]
    fn from_config_none_when_only_base_url_set() {
        let mut config = base_config();
        config.nerv_base_url = Some("https://nerv.example.internal:3335".to_string());
        assert!(NervTarget::from_config(&config).is_none());
    }

    #[test]
    fn from_config_none_when_only_token_set() {
        let mut config = base_config();
        config.nerv_bearer_token = Some("secret".to_string());
        assert!(NervTarget::from_config(&config).is_none());
    }

    #[test]
    fn from_config_some_when_both_set() {
        let mut config = base_config();
        config.nerv_base_url = Some("https://nerv.example.internal:3335".to_string());
        config.nerv_bearer_token = Some("secret".to_string());
        assert!(NervTarget::from_config(&config).is_some());
    }

    #[test]
    fn report_url_strips_trailing_slash() {
        let target = NervTarget {
            base_url: "https://nerv.example.internal:3335/".to_string(),
            bearer_token: "secret".to_string(),
        };
        assert_eq!(
            target.report_url(),
            "https://nerv.example.internal:3335/api/status/report"
        );
    }

    #[test]
    fn status_report_carries_metadata_only_no_identity() {
        let status = AgentStatus {
            state: AgentState::Working,
            last_activity_age: Some(Duration::from_secs(12)),
            last_tool: Some("Bash".to_string()),
            turn_elapsed: Some(Duration::from_secs(90)),
            grace_held: false,
            last_reply_age: None,
        };
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let report = StatusReport::from_status(&status, now);
        assert_eq!(report.state, "working");
        assert_eq!(report.last_activity_age_secs, Some(12));
        assert_eq!(report.last_tool.as_deref(), Some("Bash"));
        assert_eq!(report.turn_elapsed_secs, Some(90));
        assert_eq!(report.last_reply_age_secs, None);
        assert_eq!(report.updated_at, crate::matrix::humanize_timestamp(now));

        let json = serde_json::to_value(&report).unwrap();
        assert!(
            json.get("identity").is_none() && json.get("user_id").is_none(),
            "identity must never travel in the body — NERV maps it from the bearer token"
        );
    }

    #[test]
    fn every_agent_state_has_a_label() {
        for state in [
            AgentState::Working,
            AgentState::WaitingForUser,
            AgentState::Stalled,
            AgentState::Dead,
            AgentState::Unknown,
        ] {
            assert!(!state_label(state).is_empty());
        }
    }

    /// End-to-end: `spawn_report` against a real (local) socket, not just the pure
    /// payload-building functions above — catches anything the unit tests above can't,
    /// like a wrong path, missing auth header, or a body that doesn't actually serialize.
    #[tokio::test]
    async fn spawn_report_sends_the_expected_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
            let _ = tx.send(request);
        });

        let target = NervTarget {
            base_url: format!("http://{addr}"),
            bearer_token: "test-token".to_string(),
        };
        let report = StatusReport {
            state: "working",
            last_activity_age_secs: Some(5),
            last_tool: Some("Bash".to_string()),
            turn_elapsed_secs: Some(30),
            last_reply_age_secs: None,
            updated_at: "2026-08-28T04:00:00Z".to_string(),
        };

        spawn_report(reqwest::Client::new(), target, report);

        let request = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("mock server should have received a request within 5s")
            .expect("sender dropped without sending");
        let lower = request.to_lowercase();

        assert!(
            request.starts_with("POST /api/status/report "),
            "unexpected request line:\n{request}"
        );
        assert!(
            lower.contains("authorization: bearer test-token"),
            "missing bearer auth header:\n{request}"
        );
        assert!(
            request.contains(r#""state":"working""#),
            "body missing expected state field:\n{request}"
        );
        assert!(
            request.contains(r#""updated_at":"2026-08-28T04:00:00Z""#),
            "body missing expected updated_at field:\n{request}"
        );
        assert!(
            !request.contains("identity") && !request.contains("user_id"),
            "identity must never travel in the body:\n{request}"
        );
    }

    /// The property the module doc promises: a NERV that never responds must not delay
    /// the caller. `spawn_report` is sync and only starts a background task, so this
    /// holds by construction — this test is the guardrail against a future edit
    /// accidentally making it `async` and awaited inline.
    #[tokio::test]
    async fn spawn_report_returns_immediately_even_against_a_hanging_server() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accept the connection but never respond — simulates an unreachable/hung NERV.
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await
        });

        let target = NervTarget {
            base_url: format!("http://{addr}"),
            bearer_token: "test-token".to_string(),
        };
        let report = StatusReport {
            state: "working",
            last_activity_age_secs: None,
            last_tool: None,
            turn_elapsed_secs: None,
            last_reply_age_secs: None,
            updated_at: "2026-08-28T04:00:00Z".to_string(),
        };

        let start = std::time::Instant::now();
        spawn_report(reqwest::Client::new(), target, report);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "spawn_report must return immediately regardless of the server's behavior, took {:?}",
            start.elapsed()
        );
    }
}
