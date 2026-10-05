#![cfg(unix)]

mod sandbox;

use claudear::storage::{AttemptTracker, SqliteTracker};
use claudear::types::FixAttemptStatus;
use sandbox::Sandbox;
use std::time::Duration;
use tokio::time::timeout;

const PROCESS_WAIT: Duration = Duration::from_secs(40);
const RETRIES: &str = "retries";
const SOURCE: &str = "jira";
const ISSUE_ID: &str = "10001";
const SHORT_ID: &str = "SANDBOX-1";
const FIRST_FAILURE: &str = "the first run failed";

impl Sandbox {
    fn tracker(&self) -> SqliteTracker {
        SqliteTracker::new(self.database()).expect("open the sandbox database")
    }
}

#[tokio::test]
async fn process_keeps_a_retry_whose_source_cannot_be_reached() {
    let sandbox = Sandbox::new();
    let tracker = sandbox.tracker();
    tracker
        .record_attempt(SOURCE, ISSUE_ID, SHORT_ID)
        .expect("record the attempt");
    tracker
        .mark_failed(SOURCE, ISSUE_ID, FIRST_FAILURE)
        .expect("mark the attempt failed");
    drop(tracker);

    let status = timeout(
        PROCESS_WAIT,
        sandbox.command(RETRIES, &["retries", "process"]).status(),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "claudear retries process did not exit within {}s\n{}",
            PROCESS_WAIT.as_secs(),
            sandbox.diagnostics()
        )
    })
    .expect("run claudear retries process");

    assert!(
        status.success(),
        "claudear retries process failed with {status}\n{}",
        sandbox.diagnostics()
    );
    let attempt = sandbox
        .tracker()
        .get_attempt(SOURCE, ISSUE_ID)
        .expect("read the attempt")
        .expect("the attempt is still recorded");
    assert_eq!(
        (
            attempt.status,
            attempt.retry_count,
            attempt.error_message.as_deref()
        ),
        (FixAttemptStatus::Failed, 0, Some(FIRST_FAILURE)),
        "a retry whose source cannot be reached must keep its retry and leave the attempt \
         failed as it was, not pending where no later retry picks it up\n{}",
        sandbox.diagnostics()
    );
}
