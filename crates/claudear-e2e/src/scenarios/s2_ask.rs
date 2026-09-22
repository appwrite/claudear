//! Scenario 2: Ask flow + regression cycling.
//!
//! Discord/Slack source -> ask/question flow -> PR -> merge ->
//! simulated regression -> retry -> re-merge -> resolve.

use super::ScenarioContext;
use crate::cleanup::CleanupTracker;
use crate::config::ConfigBuilder;
use crate::db::{DbAccess, E2eDb};
use crate::{daemon, wait};
use anyhow::{bail, Context, Result};
use std::time::Duration;

const PORT: u16 = 3151;

/// Ask-flow instructions that force Claude to use the blocking_question field.
///
/// The instruction deliberately forbids making changes and requires the question
/// as a hard gate. Without the answer, Claude cannot know which framework to use
/// and therefore cannot produce a correct fix.
const ASK_INSTRUCTIONS: &str = "CRITICAL CONSTRAINT: You are FORBIDDEN from creating any commits, branches, or pull requests until you have received a human answer to your blocking question. Your FIRST structured output MUST have success=false and blocking_question populated with this exact question: 'Which testing framework should I use for this change?' with options=['pytest', 'unittest', 'none']. If you return success=true or blocking_question=null on your first attempt, the system will reject your output as invalid. You MUST ask the question first and wait for the answer before proceeding.";

pub async fn run(ctx: &ScenarioContext<'_>) -> Result<()> {
    let mut cleanup = CleanupTracker::new(ctx.scm.clone(), ctx.source.clone());
    let result = run_inner(ctx, &mut cleanup).await;
    cleanup.cleanup().await;
    result
}

async fn run_inner(ctx: &ScenarioContext<'_>, cleanup: &mut CleanupTracker) -> Result<()> {
    let ask = ctx
        .ask_backend
        .as_ref()
        .context("S2 requires an ask backend")?;

    let tmp_dir = tempfile::tempdir().context("create temp dir")?;
    let workspace = tmp_dir.path().join("workdir");
    let repos_dir = tmp_dir.path().join("repos");
    let log_dir = tmp_dir.path().join("logs");
    let db_path = tmp_dir.path().join("s2.db");

    std::fs::create_dir_all(&repos_dir).context("create repos dir")?;

    // Clone the repo locally so the daemon's inferrer can discover it
    tracing::info!("Cloning repo for daemon discovery");
    ctx.clone_repo(ctx.repo, &repos_dir)?;

    let mut builder = ConfigBuilder::new(&workspace, &db_path, PORT)
        .claude_timeout(ctx.claude_timeout)
        .skip_permissions()
        .instructions(ASK_INSTRUCTIONS)
        .retry(3)
        .regression(true)
        .ask(true);

    if ctx.use_docker {
        builder = builder
            .docker_paths()
            .auto_discover_paths(vec!["/app/repos".to_string()]);
    } else {
        builder = builder.auto_discover_paths(vec![repos_dir.to_string_lossy().to_string()]);
    }

    // Configure SCM backend
    match ctx.scm_name {
        "github" => {
            let token =
                std::env::var("CLAUDEAR_E2E_GITHUB_TOKEN").context("GITHUB_TOKEN required")?;
            builder = builder.github(&token, ctx.repo);
        }
        "gitlab" => {
            let token =
                std::env::var("CLAUDEAR_E2E_GITLAB_TOKEN").context("GITLAB_TOKEN required")?;
            let base_url = std::env::var("CLAUDEAR_E2E_GITLAB_URL")
                .unwrap_or_else(|_| "https://gitlab.com".to_string());
            let group = ctx.repo.split('/').next().unwrap_or(ctx.repo);
            builder = builder.gitlab(&token, &base_url, group);
        }
        _ => {}
    }

    // Configure ask source backend (Discord or Slack) — this IS the issue source for S2.
    // No Jira/Linear source: the daemon detects issues from messages posted in the ask channel.
    match ctx.ask_name {
        "discord" => {
            let bot_token =
                std::env::var("CLAUDEAR_E2E_DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN")?;
            let channel_id =
                std::env::var("CLAUDEAR_E2E_DISCORD_CHANNEL_ID").context("DISCORD_CHANNEL_ID")?;
            builder = builder.discord_source(&bot_token, &channel_id);
        }
        "slack" => {
            let bot_token =
                std::env::var("CLAUDEAR_E2E_SLACK_BOT_TOKEN").context("SLACK_BOT_TOKEN")?;
            let channel_id =
                std::env::var("CLAUDEAR_E2E_SLACK_CHANNEL_ID").context("SLACK_CHANNEL_ID")?;
            builder = builder.slack_source(&bot_token, &channel_id);

            // Resolve the bot's user ID so the daemon's Slack notifier accepts
            // thread replies from our bot (which we use to simulate user replies).
            if let Some(slack_ask) = ask.as_any().downcast_ref::<crate::ask::SlackAsk>() {
                if let Ok(bot_user_id) = slack_ask.resolve_bot_user_id().await {
                    tracing::info!(bot_user_id = %bot_user_id, "Resolved Slack bot user ID");
                    builder = builder.slack_user_id(&bot_user_id);
                }
            }
        }
        _ => {}
    }

    // Configure notifier for the ask backend.
    // Discord: no explicit notifier config needed — discord_merged() pulls bot_token
    // + channel_id from issues.discord, so send() uses the bot path automatically.
    // This matches the real-world pattern: webhook posts issues, bot sends notifications.
    if ctx.ask_name == "slack" {
        if let (Ok(bot_token), Ok(channel_id)) = (
            std::env::var("CLAUDEAR_E2E_SLACK_BOT_TOKEN"),
            std::env::var("CLAUDEAR_E2E_SLACK_CHANNEL_ID"),
        ) {
            builder = builder.slack_notifier(&bot_token, &channel_id);
        }
    }

    let config_path = builder.write_to(tmp_dir.path(), "s2")?;

    // Discord/Slack sources use cursor-based polling: the first poll seeds the
    // cursor at the latest message and returns nothing. We MUST wait for this
    // before posting the issue message, otherwise it will be permanently skipped.
    tracing::info!("S2-A: Starting daemon (cursor-based source — must seed before posting)");

    let mut handle = if ctx.use_docker {
        daemon::start_docker(
            ctx.docker_image,
            &config_path,
            PORT,
            &log_dir,
            "s2",
            None,
            Some(&repos_dir),
            true, // reset volume for fresh start
        )?
    } else {
        let binary = ctx.binary_path()?;
        daemon::start_process(&binary, &config_path, PORT, &log_dir, "s2")?
    };

    daemon::wait_healthy(&handle, PORT, Duration::from_secs(30)).await?;

    let db = E2eDb::new(if ctx.use_docker {
        DbAccess::docker(
            "claudear-e2e-s2",
            handle.volume_name().unwrap_or("claudear-e2e-db-3151"),
        )
    } else {
        DbAccess::direct(&db_path)
    });

    // Wait for the source cursor to be seeded before posting the issue message
    tracing::info!("S2-A: Waiting for cursor seed");
    wait::wait_for_log_message(handle.log_path(), "seeded cursor", Duration::from_secs(60))
        .await
        .context("wait for cursor seed in daemon log")?;

    tracing::info!("S2-B: Posting issue via ask channel");
    let issue_msg_content =
        "[E2E-S2] Please add a hello world comment to README.md in the test repo";
    let issue_msg_id = ask.post_issue_message(issue_msg_content).await?;
    let t_issue_posted = std::time::Instant::now();
    tracing::info!(msg_id = %issue_msg_id, "Posted issue message");

    tracing::info!("S2-C: Waiting for detection");

    // Wait for fix_attempts row matching our message
    let source_name = ctx.ask_name;
    let esc_msg_id = issue_msg_id.replace('\'', "''");
    wait::wait_for(
        "fix_attempts row for S2",
        ctx.timeout(),
        ctx.poll_interval(),
        || async {
            let sql = format!(
                "SELECT COUNT(*) FROM fix_attempts WHERE source='{}' AND issue_id='{}'",
                source_name, esc_msg_id
            );
            Ok(db.count(&sql).unwrap_or(0) > 0)
        },
    )
    .await?;

    tracing::info!("S2-D: Waiting for ask question");
    let question_timeout = Duration::from_secs(ctx.wait_timeout.min(300));
    let question_id = ask
        .poll_for_question(&issue_msg_id, question_timeout)
        .await
        .context("poll for ask question")?;

    let t_question_detected = std::time::Instant::now();
    tracing::info!(question_id = %question_id, "Got ask question");

    tracing::info!("S2-E: Replying to question");
    ask.reply_to_question(&question_id, "none")
        .await
        .context("reply to question")?;
    let t_reply_sent = std::time::Instant::now();

    tracing::info!("S2-F: Waiting for PR");

    wait::wait_for(
        "PR URL in fix_attempts",
        ctx.timeout(),
        ctx.poll_interval(),
        || async {
            let sql = format!(
                "SELECT COUNT(*) FROM fix_attempts WHERE source='{}' AND issue_id='{}' AND pr_url IS NOT NULL AND pr_url != ''",
                source_name, esc_msg_id
            );
            Ok(db.count(&sql).unwrap_or(0) > 0)
        },
    )
    .await?;

    let pr_url = db.query(&format!(
        "SELECT pr_url FROM fix_attempts WHERE source='{}' AND issue_id='{}' AND pr_url IS NOT NULL LIMIT 1",
        source_name, esc_msg_id
    ))?;
    let pr_url = pr_url.trim().to_string();

    if pr_url.is_empty() {
        bail!("PR URL is empty for S2");
    }

    let pr_number = ctx
        .scm
        .parse_pr_number(&pr_url)
        .context("parse PR number")?;

    cleanup.track_pr(ctx.repo, pr_number);

    let pr_branch = ctx
        .scm
        .get_pr_branch(ctx.repo, pr_number)
        .await
        .unwrap_or_default();
    if !pr_branch.is_empty() {
        cleanup.track_branch(ctx.repo, &pr_branch);
    }

    let t_pr_created = std::time::Instant::now();
    tracing::info!(pr_url = %pr_url, pr_number, "PR created");

    // Verify PR exists on the SCM
    if pr_branch.is_empty() {
        tracing::warn!(pr_number, "PR branch is empty — PR may not exist on SCM");
    } else {
        tracing::info!(pr_number, branch = %pr_branch, "PR verified on SCM");
    }

    // Verify timestamp ordering: issue posted < question < reply < PR
    assert!(
        t_issue_posted < t_question_detected,
        "T1 (issue posted) must precede T2 (question detected)"
    );
    assert!(
        t_question_detected < t_reply_sent,
        "T2 (question detected) must precede T3 (reply sent)"
    );
    assert!(
        t_reply_sent < t_pr_created,
        "T3 (reply sent) must precede T4 (PR created)"
    );
    tracing::info!(
        t1_issue_ms = t_issue_posted.elapsed().as_millis() as u64,
        t2_question_ms = t_question_detected.elapsed().as_millis() as u64,
        t3_reply_ms = t_reply_sent.elapsed().as_millis() as u64,
        t4_pr_ms = t_pr_created.elapsed().as_millis() as u64,
        "Timestamp ordering verified: T1 < T2 < T3 < T4"
    );

    // Discord/Slack sources don't have native labels, but regression watch
    // creation checks is_bug() which requires the "bug" label.
    tracing::info!("Adding 'bug' label to fix_attempts for regression watch creation");
    db.exec(&format!(
        "UPDATE fix_attempts SET issue_labels='[\"bug\"]' WHERE source='{}' AND issue_id='{}'",
        source_name, esc_msg_id
    ))?;

    tracing::info!("S2-G: Merging PR");
    ctx.scm
        .merge_pr(ctx.repo, pr_number)
        .await
        .context("merge PR")?;

    tracing::info!("S2-H: Waiting for regression watch");
    wait::wait_for(
        "regression_watches row",
        ctx.timeout(),
        ctx.poll_interval(),
        || async {
            let sql = format!(
                "SELECT COUNT(*) FROM regression_watches WHERE issue_id='{}'",
                esc_msg_id
            );
            Ok(db.count(&sql).unwrap_or(0) > 0)
        },
    )
    .await?;

    // Mirrors the bash script exactly: stop daemon → DB manipulation →
    // git reset → delete PR branch → restart daemon.
    tracing::info!("S2-I: Simulating regression");

    // 1. Stop daemon first — the retry manager only checks for failed attempts
    //    on startup, so we must restart after our DB changes.
    tracing::info!("Stopping daemon for regression simulation");
    daemon::stop(&mut handle);

    // 2. Set regression_watches to monitoring (backdated)
    db.exec(&format!(
        "UPDATE regression_watches SET status='monitoring', monitoring_started_at=datetime('now', '-20 seconds') WHERE issue_id='{esc_msg_id}'"
    ))?;

    // 3. Get watch ID and insert regression check
    let watch_id = db.query(&format!(
        "SELECT id FROM regression_watches WHERE issue_id='{esc_msg_id}' ORDER BY id DESC LIMIT 1"
    ))?;
    let watch_id = watch_id.trim();

    if !watch_id.is_empty() {
        db.exec(&format!(
            "INSERT INTO regression_checks (regression_watch_id, issue_still_exists, check_details, checked_at) VALUES ({watch_id}, 1, 'Simulated regression detected by e2e test', datetime('now'))"
        ))?;

        // 4. Set watch to regressed (mirrors what the daemon does when it detects
        // the issue still exists during monitoring)
        db.exec(&format!(
            "UPDATE regression_watches SET status='regressed', regressed_at=datetime('now') WHERE id={watch_id}"
        ))?;
    }

    // 6. Reset fix_attempts: clear pr_url so retry creates a new PR
    db.exec(&format!(
        "UPDATE fix_attempts SET status='failed', pr_url=NULL, scm_pr_number=NULL, error_message='Regression detected (simulated)' WHERE source='{}' AND issue_id='{esc_msg_id}'",
        source_name
    ))?;

    // 7. Mark any other fix_attempts from this source as 'ignored' so the retry
    //    manager only retries our test message (stale notification messages may
    //    have created attempts)
    db.exec(&format!(
        "UPDATE fix_attempts SET status='ignored' WHERE source='{}' AND issue_id != '{esc_msg_id}' AND status IN ('failed', 'closed')",
        source_name
    ))?;

    // Verify DB state after regression simulation: status='failed', retry_count=0, no PR
    {
        let status = db.query(&format!(
            "SELECT status FROM fix_attempts WHERE source='{}' AND issue_id='{esc_msg_id}' LIMIT 1",
            source_name
        ))?;
        let status = status.trim();
        assert!(
            status == "failed",
            "Expected status='failed' after regression sim, got '{}'",
            status
        );

        let retry_count = db.query(&format!(
            "SELECT retry_count FROM fix_attempts WHERE source='{}' AND issue_id='{esc_msg_id}' LIMIT 1",
            source_name
        ))?;
        let retry_count = retry_count.trim();
        assert!(
            retry_count == "0",
            "Expected retry_count=0 after regression sim, got '{}'",
            retry_count
        );

        let pr_check = db.query(&format!(
            "SELECT COALESCE(pr_url, '') FROM fix_attempts WHERE source='{}' AND issue_id='{esc_msg_id}' LIMIT 1",
            source_name
        ))?;
        let pr_check = pr_check.trim();
        assert!(
            pr_check.is_empty(),
            "Expected pr_url=NULL after regression sim, got '{}'",
            pr_check
        );

        tracing::info!("DB state verified: status=failed, retry_count=0, pr_url=NULL");
    }

    // 8. Reset main on GitHub to pre-merge state (remove squash-merged changes)
    //    This prevents merge conflicts when Claude creates the retry PR.
    //    Using a revert commit would leave history that confuses Claude into thinking
    //    the fix was intentionally reverted, causing it to skip PR creation.
    let repo_name = ctx.repo.rsplit('/').next().unwrap_or(ctx.repo);
    let local_repo = repos_dir.join(repo_name);

    tracing::info!("Resetting main to pre-merge state");
    let git = |args: &[&str]| -> Result<()> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&local_repo)
            .output()
            .context("git command")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(args = ?args, stderr = %stderr, "git command failed");
        }
        Ok(())
    };

    git(&["fetch", "origin", "main"])?;
    git(&["checkout", "main"])?;
    git(&["reset", "--hard", "origin/main"])?;
    git(&["reset", "--hard", "HEAD~1"])?;
    git(&["push", "--force", "origin", "main"])?;
    tracing::info!("Reset main to pre-merge state (force pushed)");

    // 9. Delete old PR branch so Claude can create a fresh one
    if !pr_branch.is_empty() {
        let _ = ctx.scm.delete_branch(ctx.repo, &pr_branch).await;
        tracing::info!(branch = %pr_branch, "Deleted old PR branch");
    }

    // 10. Restart daemon (reuses same volume so DB state is preserved)
    tracing::info!("Restarting daemon for retry");
    handle = if ctx.use_docker {
        daemon::start_docker(
            ctx.docker_image,
            &config_path,
            PORT,
            &log_dir,
            "s2",
            Some(handle.volume_name().unwrap_or("claudear-e2e-db-3151")),
            Some(&repos_dir),
            false, // preserve volume for regression retry
        )?
    } else {
        let binary = ctx.binary_path()?;
        daemon::start_process(&binary, &config_path, PORT, &log_dir, "s2")?
    };

    daemon::wait_healthy(&handle, PORT, Duration::from_secs(30)).await?;

    // Wait for repo discovery to confirm bind mount is working
    tracing::info!("Waiting for repo discovery after restart");
    wait::wait_for_log_message(
        handle.log_path(),
        "Scanning for repositories",
        Duration::from_secs(60),
    )
    .await
    .context("wait for repo scanning after restart")?;

    // Wait for the retry manager to find our failed attempt
    tracing::info!("Waiting for retry manager to pick up failed attempt");
    wait::wait_for_log_message(
        handle.log_path(),
        "Processing ready retries",
        Duration::from_secs(60),
    )
    .await
    .context("wait for retry processing after restart")?;

    // The retry will also trigger an ask question (same instructions). Poll for it
    // and reply before waiting for the retry PR.
    tracing::info!("S2-I: Polling for retry ask question");
    match ask
        .poll_for_question(&question_id, Duration::from_secs(120))
        .await
    {
        Ok(retry_q_id) => {
            tracing::info!(question_id = %retry_q_id, "Got retry ask question");
            ask.reply_to_question(&retry_q_id, "none")
                .await
                .context("reply to retry ask question")?;
            tracing::info!("Replied to retry ask question");
        }
        Err(e) => {
            tracing::warn!(error = %e, "No retry ask question found (best_effort_on_timeout will handle)");
        }
    }

    tracing::info!("S2-I: Waiting for retry PR");
    wait::wait_for(
        "retry fix_attempts row",
        ctx.timeout(),
        ctx.poll_interval(),
        || async {
            let sql = format!(
                "SELECT COUNT(*) FROM fix_attempts WHERE source='{}' AND issue_id = '{esc_msg_id}' AND retry_count >= 1 AND pr_url IS NOT NULL AND pr_url != ''",
                source_name
            );
            Ok(db.count(&sql).unwrap_or(0) > 0)
        },
    )
    .await?;

    // Get the retry PR URL (same row, updated)
    let retry_pr_url = db.query(&format!(
        "SELECT pr_url FROM fix_attempts WHERE source='{}' AND issue_id = '{esc_msg_id}' AND pr_url IS NOT NULL LIMIT 1",
        source_name
    ))?;
    let retry_pr_url = retry_pr_url.trim().to_string();

    if !retry_pr_url.is_empty() {
        // Verify retry DB state: retry_count >= 1 and new PR differs from original
        let retry_count_str = db.query(&format!(
            "SELECT retry_count FROM fix_attempts WHERE source='{}' AND issue_id='{esc_msg_id}' LIMIT 1",
            source_name
        ))?;
        let retry_count_val: u32 = retry_count_str.trim().parse().unwrap_or(0);
        assert!(
            retry_count_val >= 1,
            "Expected retry_count >= 1 after retry, got {}",
            retry_count_val
        );
        assert_ne!(
            retry_pr_url, pr_url,
            "Retry PR URL should differ from original"
        );
        tracing::info!(retry_count = retry_count_val, "Retry DB state verified");

        let retry_pr_number = ctx
            .scm
            .parse_pr_number(&retry_pr_url)
            .context("parse retry PR number")?;

        cleanup.track_pr(ctx.repo, retry_pr_number);

        let retry_pr_branch = ctx
            .scm
            .get_pr_branch(ctx.repo, retry_pr_number)
            .await
            .unwrap_or_default();
        if !retry_pr_branch.is_empty() {
            cleanup.track_branch(ctx.repo, &retry_pr_branch);
            tracing::info!(pr_number = retry_pr_number, branch = %retry_pr_branch, "Retry PR verified on SCM");
        }

        tracing::info!(retry_pr_url = %retry_pr_url, retry_pr_number, "Retry PR created");

        // Re-add bug label (it was cleared during reset)
        db.exec(&format!(
            "UPDATE fix_attempts SET issue_labels='[\"bug\"]' WHERE source='{}' AND issue_id='{esc_msg_id}'",
            source_name
        ))?;

        // Merge retry PR (wait for GitHub to compute mergeability)
        tracing::info!("S2-I: Merging retry PR");
        let mut merged = false;
        for attempt in 0..6 {
            match ctx.scm.merge_pr(ctx.repo, retry_pr_number).await {
                Ok(()) => {
                    merged = true;
                    break;
                }
                Err(e) if attempt < 5 => {
                    tracing::warn!(attempt, error = %e, "Retry PR not yet mergeable, waiting...");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
                Err(e) => return Err(e).context("merge retry PR"),
            }
        }
        if !merged {
            anyhow::bail!("Failed to merge retry PR after retries");
        }
    }

    tracing::info!("S2-J: Verifying assertions");

    db.assert_min_count(
        "fix_attempts",
        &format!(
            "SELECT COUNT(*) FROM fix_attempts WHERE source='{}' AND issue_id='{esc_msg_id}'",
            source_name
        ),
        1,
    )?;

    db.assert_min_count(
        "claude_executions",
        "SELECT COUNT(*) FROM claude_executions",
        1,
    )?;

    db.assert_min_count(
        "regression_checks",
        "SELECT COUNT(*) FROM regression_checks WHERE issue_still_exists=1",
        1,
    )?;

    daemon::stop(&mut handle);

    tracing::info!("S2: All checkpoints passed!");
    Ok(())
}
