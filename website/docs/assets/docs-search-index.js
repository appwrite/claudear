window.__CLAUDEAR_DOCS_SEARCH_INDEX__ = [
  {
    slug: "index",
    title: "Quickstart",
    description: "From install to your first automated fix in 5 minutes.",
    headings: [
      { id: "install", text: "Install", level: 3 },
      { id: "configure", text: "Configure", level: 3 },
      { id: "seed", text: "Seed", level: 3 },
      { id: "run", text: "Run", level: 3 },
      { id: "verify-it-works", text: "Verify it works", level: 2 },
      { id: "whats-next", text: "What's next", level: 2 }
    ],
    text: "Quickstart from install to your first automated fix in 5 minutes. Prerequisites Claude Code CLI installed and authenticated, a GitHub or GitLab token for creating PRs, an issue tracker token Linear Sentry Jira. Install via Homebrew brew tap abnegate/tap brew install claudear. APT Debian Ubuntu prebuilt binaries from source Docker. Configure claudear.toml minimal config workspace known_orgs agent providers claude model sonnet scm github token issues linear api_key trigger_labels notifiers discord webhook_url. Seed mark existing issues as seen claudear seed. Run claudear start poll port 3100. Verify it works claudear dry-run create a labelled issue confirm the full cycle issue picked up PR opened notification sent dashboard shows the attempt. Recipes Linear GitHub Discord Sentry GitHub Slack GitHub Issues GitHub Discord Connect your tools."
  },
  {
    slug: "getting-started",
    title: "Recipes",
    description: "Complete configurations for popular stacks. Copy, paste, fill in your tokens.",
    headings: [
      { id: "how-to-use", text: "How to use these recipes", level: 2 },
      { id: "recipe-linear", text: "Linear + GitHub + Discord", level: 2 },
      { id: "recipe-sentry", text: "Sentry + GitHub + Slack", level: 2 },
      { id: "recipe-github", text: "GitHub Issues + GitHub + Discord", level: 2 },
      { id: "recipe-jira", text: "Jira + GitLab + Email", level: 2 },
      { id: "customising", text: "Customising recipes", level: 2 },
      { id: "add-notification-channel", text: "Add a second notification channel", level: 3 },
      { id: "enable-retry", text: "Enable retry with backoff", level: 3 },
      { id: "add-user-mapping", text: "Add user mapping", level: 3 },
      { id: "switch-to-webhooks", text: "Switch to webhooks", level: 3 }
    ],
    text: "Complete configurations for popular stacks. Copy paste fill in your tokens. Linear GitHub Discord recipe full working TOML config api_key trigger_labels webhook_url. Sentry GitHub Slack recipe auth_token org_slug project_slugs min_event_count bot_token channel_id. GitHub Issues GitHub Discord recipe repos trigger_labels pure GitHub workflow. Jira GitLab Email recipe base_url email api_token project_keys trigger_labels smtp. Customising recipes add a second notification channel enable retry with backoff max_retries base_delay_ms max_delay_ms add user mapping linear_name github_username discord_id switch to webhooks claudear webhook setup base-url."
  },
  {
    slug: "integrations",
    title: "Connect your tools",
    description: "Add issue sources, source control providers, and notification channels.",
    headings: [
      { id: "issue-sources", text: "Issue sources", level: 2 },
      { id: "linear", text: "Linear", level: 3 },
      { id: "sentry", text: "Sentry", level: 3 },
      { id: "jira", text: "Jira", level: 3 },
      { id: "github-issues", text: "GitHub Issues", level: 3 },
      { id: "gitlab-issues", text: "GitLab Issues", level: 3 },
      { id: "messaging-sources", text: "Discord / Slack as sources", level: 3 },
      { id: "source-control", text: "Source control", level: 2 },
      { id: "github-pat", text: "GitHub (PAT)", level: 3 },
      { id: "github-app", text: "GitHub App", level: 3 },
      { id: "gitlab", text: "GitLab", level: 3 },
      { id: "notifications", text: "Notifications", level: 2 },
      { id: "reply-capable", text: "Reply-capable (recommended)", level: 3 },
      { id: "delivery-only", text: "Delivery-only", level: 3 },
      { id: "telemetry", text: "Telemetry", level: 2 },
      { id: "grafana", text: "Grafana", level: 3 },
      { id: "mcp-options", text: "MCP server options", level: 3 },
      { id: "user-mapping", text: "User mapping", level: 2 },
      { id: "webhooks", text: "Webhooks", level: 2 }
    ],
    text: "Connect your tools. Add issue sources source control providers and notification channels. Linear picks up issues matching label state triggers api_key trigger_labels trigger_states. Sentry monitors errors above event count threshold auth_token org_slug project_slugs min_event_count. Jira Cloud and Server Data Center filter by project label custom JQL base_url email api_token project_keys trigger_labels. GitHub Issues repos trigger_labels. GitLab Issues groups trigger_labels. Discord Slack messages as issues. Source control GitHub Personal Access Token quickest setup token auto_resolve_on_merge. GitHub App for organisations app_id private_key_path installation_id. GitLab personal access token base_url. Notifications reply-capable recommended Discord bot_token channel_id Slack bot_token channel_id Email SMTP IMAP. Delivery-only SMS Push WhatsApp Telegram. Telemetry MCP servers give the agent live context while triaging. Grafana mcp-grafana Prometheus metrics Loki logs datasources service account token GRAFANA_URL GRAFANA_SERVICE_ACCOUNT_TOKEN disable-write viewer role. MCP server options command args stdio url headers type http sse sources gating tools allowlist instructions prompt guidance env expansion. User mapping cross-platform identities linear_name github_username discord_id email. Webhooks claudear webhook setup base-url react to events instantly."
  },
  {
    slug: "regression",
    title: "Regression monitoring",
    description: "Detect when a merged fix regresses — Sentry event matching, similarity thresholds, and configuration.",
    headings: [
      { id: "what-it-does", text: "What it does", level: 2 },
      { id: "how-it-works", text: "How it works", level: 2 },
      { id: "configuration", text: "Configuration", level: 2 },
      { id: "cli-commands", text: "CLI commands", level: 2 },
      { id: "tips", text: "Tips", level: 2 }
    ],
    text: "Regression monitoring detect when a merged fix regresses. After a fix is merged Claudear watches for the error to reappear in Sentry. If the same or semantically similar error surfaces above a threshold it is flagged as a regression and a new fix attempt is queued. Monitoring window opens when PR merges. Periodic checks every check_interval_hours. Similarity matching semantic similarity threshold. Sentry event threshold. Configuration regression enabled check_interval_hours monitoring_duration_hours sentry_event_threshold similarity_threshold target_repos github_token github_search_repos. CLI commands claudear regressions list claudear regressions check. Tips monitoring_duration_hours similarity_threshold target_repos Dashboard Regressions page."
  },
  {
    slug: "cascading",
    title: "Cascading fixes",
    description: "Automatically propagate fixes across dependent repositories with dependency chains and cascade rules.",
    headings: [
      { id: "what-it-does", text: "What it does", level: 2 },
      { id: "how-dependency-chains-work", text: "How dependency chains work", level: 2 },
      { id: "configuration", text: "Configuration", level: 2 },
      { id: "example", text: "Example", level: 2 },
      { id: "cli-commands", text: "CLI commands", level: 2 },
      { id: "tips", text: "Tips", level: 2 }
    ],
    text: "Cascading fixes propagate fixes across dependent repositories. A fix in a shared library triggers follow-up PRs in downstream repos. Declare dependencies with claudear repos link. Upstream fix merges and Claudear walks the dependency graph. Downstream PRs opened with version updates. Depth control max_depth. Configuration cascade enabled max_depth cascade.rules upstream downstream trigger release merge target_branch version_update instructions. CLI commands claudear repos link claudear repos cascade claudear repos graph. Example library fix triggers application update."
  },
  {
    slug: "learning",
    title: "Learning from results",
    description: "How Claudear extracts knowledge from diffs, Q&A, and review comments to improve future fixes.",
    headings: [
      { id: "what-it-does", text: "What it does", level: 2 },
      { id: "knowledge-sources", text: "Knowledge sources", level: 2 },
      { id: "auto-agent-md", text: "Auto AGENT.md", level: 2 },
      { id: "cluster-detection", text: "Cluster detection", level: 2 },
      { id: "configuration", text: "Configuration", level: 2 },
      { id: "cli-commands", text: "CLI commands", level: 2 }
    ],
    text: "Learning from results. Claudear analyses PR diffs review comments Q&A answers and execution logs to build per-repo knowledge. Knowledge sources: diff_analysis qa_promotion review_classification auto_extract_learnings strategy_fingerprinting quality_scoring. Auto AGENT.md auto_agent_md generates AGENT.md per repo from accumulated knowledge. Cluster detection groups correlated issues. Configuration learning auto_extract_learnings diff_analysis qa_promotion qa_promotion_threshold repo_knowledge review_classification review_promotion_threshold strategy_fingerprinting quality_scoring cluster_detection cluster_window_minutes min_cluster_size auto_agent_md. CLI commands claudear learn show."
  },
  {
    slug: "prioritisation",
    title: "Issue prioritisation",
    description: "Scoring model, blast radius classification, content clustering, and suppression rules.",
    headings: [
      { id: "scoring-model", text: "Scoring model", level: 2 },
      { id: "blast-radius", text: "Blast radius classification", level: 2 },
      { id: "content-clustering", text: "Content clustering", level: 2 },
      { id: "suppression-rules", text: "Suppression rules", level: 2 },
      { id: "configuration", text: "Configuration", level: 2 }
    ],
    text: "Issue prioritisation scoring model blast radius classification content clustering suppression rules. Composite severity score from severity frequency regression blast_radius cluster weights. Blast radius tiers: critical auth payment billing security login oauth, core api middleware router handler, infra deploy docker terraform k8s database migration, test spec fixture mock, cosmetic readme changelog license docs. Content clustering groups similar issues by error type culprit title. Suppression rules skip known-noisy issues field pattern match_mode contains regex. Configuration prioritisation enabled severity_weight frequency_weight regression_weight blast_radius_weight cluster_weight critical_paths core_paths infra_paths test_paths cosmetic_paths content_clustering cluster_similarity_threshold min_content_cluster_size."
  },
  {
    slug: "repositories",
    title: "Repository management",
    description: "Repo discovery, code indexing with tree-sitter, self-evaluation, and dependency graphs.",
    headings: [
      { id: "discovery-and-indexing", text: "Discovery and indexing", level: 2 },
      { id: "code-indexing", text: "Code indexing", level: 2 },
      { id: "self-evaluation", text: "Self-evaluation", level: 2 },
      { id: "dashboard-config", text: "Dashboard config", level: 2 },
      { id: "dependency-graphs", text: "Dependency graphs", level: 2 },
      { id: "cli-commands", text: "CLI commands", level: 2 }
    ],
    text: "Repository management discovery indexing dependencies search. Auto-discover repos from GitHub GitLab organisations. Code indexing with tree-sitter for semantic search. Self-evaluation runs before/after comparisons tests lint static analysis coverage. Dashboard config cost estimation max_plan_monthly_cost hourly_engineer_rate. Dependency graphs for cascading fixes and issue routing. CLI commands claudear repos discover index search graph link stats sync. Configuration code_index enabled max_file_size_kb batch_size. Evaluation enabled test_delta lint_delta static_analysis_delta coverage_delta tool_timeout_secs total_timeout_secs post_pr_comment fail_on_regression."
  },
  {
    slug: "usage",
    title: "How it works",
    description: "The fix lifecycle, retries, AI questions, and day-to-day workflows.",
    headings: [
      { id: "the-fix-pipeline", text: "The fix pipeline", level: 2 },
      { id: "retries", text: "Retries", level: 2 },
      { id: "pr-monitoring", text: "PR monitoring", level: 2 },
      { id: "ai-questions", text: "AI questions", level: 2 },
      { id: "workflows", text: "Day-to-day workflows", level: 2 },
      { id: "advanced-features", text: "Advanced features", level: 2 }
    ],
    text: "How it works the fix lifecycle retries AI questions and day-to-day workflows. The fix pipeline: Pending picked up and queued, Success AI created a pull request, Merged PR merged, Closed PR closed without merging triggers retry, Failed attempt failed triggers retry, Cannot Fix retry budget exhausted flagged for manual review. Retries with exponential backoff max_retries base_delay_ms max_delay_ms claudear retries list process. PR monitoring auto-resolve on merge claudear prs list monitor continuous. AI questions clarification through Discord Slack email reply and fix resumes timeout best_effort_on_timeout remembers previous answers. Day-to-day workflows check status activity trigger manual fix claudear trigger reset retry report preview send. Advanced features regression monitoring cascading fixes learning from results issue prioritisation repository management repos discover index search graph."
  },
  {
    slug: "dashboard",
    title: "Dashboard",
    description: "Monitor fix attempts, track issues, and manage Claudear from a web dashboard.",
    headings: [
      { id: "opening-the-dashboard", text: "Opening the dashboard", level: 2 },
      { id: "what-youll-see", text: "What you'll see", level: 2 },
      { id: "screenshots", text: "Screenshots", level: 2 }
    ],
    text: "Opening the dashboard: claudear dashboard on default port 3100 or custom port 8080. Also available when running claudear start --poll --port 3100. What you will see: Overview at-a-glance metrics fix attempts today success rate active sources recent activity. Attempts full history of every fix attempt with status source repo timing and AI execution log. Issues all issues ingested across sources with current status. Pull Requests every PR MR opened with merge status. Regressions post-fix monitoring flags if error reappears after merge. Errors issues that failed and why. Analytics trends over time fix rate merge rate source breakdown latency. Repos repositories and indexing status. Inference routing issues to right repo with confidence scores. Learning what Claudear learned from previous fixes per repo. Activity live feed of everything happening now. Settings config and user management. The dashboard is an admin interface. In production deploy behind authentication or restrict to internal network."
  },
  {
    slug: "operations",
    title: "Deploy",
    description: "Run Claudear as a background service, in Docker, or in CI.",
    headings: [
      { id: "deployment-checklist", text: "Deployment checklist", level: 2 },
      { id: "background-daemon", text: "Background daemon", level: 2 },
      { id: "macos-launchd", text: "macOS (launchd)", level: 2 },
      { id: "linux-systemd", text: "Linux (systemd)", level: 2 },
      { id: "docker", text: "Docker", level: 2 },
      { id: "compose", text: "Compose (recommended)", level: 3 },
      { id: "standalone", text: "Standalone", level: 3 },
      { id: "environment-variables", text: "Environment variables", level: 2 },
      { id: "tips-for-production", text: "Tips for production", level: 2 }
    ],
    text: "Deploy run Claudear as a background service in Docker or in CI. Deployment checklist seed confirm dashboard trigger test issue tune max_concurrent enable regression monitoring reports. Background daemon claudear start --poll --port 3100 status pause resume stop. macOS launchd WorkingDirectory EnvironmentVariables KeepAlive. Linux systemd EnvironmentFile Restart on-failure. Docker compose recommended docker compose up -d logs down. Standalone docker run claudear-data volume port 3100. Health check GET /api/health returns 200. Environment variables LINEAR_API_KEY SENTRY_AUTH_TOKEN GITHUB_TOKEN JIRA_API_TOKEN DISCORD_BOT_TOKEN SLACK_BOT_TOKEN. Tips for production log rotation reply-capable notifications health monitoring conservative max_concurrent."
  },
  {
    slug: "configuration",
    title: "Configuration",
    description: "Every setting in claudear.toml. Start with the Quickstart — come here when you need to tune.",
    headings: [
      { id: "config-file", text: "Config file", level: 2 },
      { id: "environment-variables", text: "Environment variables", level: 2 },
      { id: "core-settings", text: "Core settings", level: 2 },
      { id: "retries", text: "Retries", level: 2 },
      { id: "ai-agent", text: "AI agent", level: 2 },
      { id: "user-mapping", text: "User mapping", level: 2 },
      { id: "questions", text: "Questions", level: 2 },
      { id: "source-control", text: "Source control", level: 2 },
      { id: "issue-sources", text: "Issue sources", level: 2 },
      { id: "notifications", text: "Notifications", level: 2 },
      { id: "advanced-features", text: "Advanced features", level: 2 },
      { id: "minimal-config", text: "Minimal config", level: 2 },
      { id: "full-annotated-example-config-appendix", text: "Full annotated example config", level: 2 }
    ],
    text: "Configuration reference every setting in claudear.toml. Config file cp claudear.example.toml claudear.toml override path with --config. Environment variables for secrets: LINEAR_API_KEY, SENTRY_AUTH_TOKEN, GITHUB_TOKEN, LINEAR_WEBHOOK_SECRET, SENTRY_CLIENT_SECRET, GITHUB_WEBHOOK_SECRET, EMBEDDING_MODEL, EMBEDDING_CACHE_DIR. Core settings: workspace where repos are cloned, db_path SQLite database, webhook_port, poll_interval_ms, max_concurrent, known_orgs, auto_discover_paths. Retries with exponential backoff: max_retries, base_delay_ms, max_delay_ms. AI agent config: default_provider, timeout_secs. Claude provider: model sonnet opus haiku, instructions, instructions_file, permissions, skip_permissions. User mapping across platforms: linear_name, github_username, sentry_username, jira_username, discord_id, slack_id, email. Questions ask loop: enabled, wait_timeout_secs, poll_interval_secs, max_rounds_per_attempt, best_effort_on_timeout, semantic reuse thresholds. Source control GitHub: token, auto_resolve_on_merge, review_trigger, use_ssh, webhook_secret. GitHub App: app_id, private_key_path, installation_id. GitLab: token, base_url, groups, trigger_labels, trigger_states. Issue sources Linear: api_key, trigger_labels, trigger_states, trigger_assignee. Sentry: auth_token, org_slug, project_slugs, min_event_count, escalation_threshold_percent. Jira: base_url, email, api_token, auth_mode basic bearer, project_keys, trigger_labels, trigger_statuses, custom_jql. Discord and Slack as issue sources. Notifications: Discord webhook_url bot_token channel_id. Slack bot_token channel_id. Email SMTP IMAP. SMS Twilio. Push Pushover. WhatsApp Telegram. Advanced: regression monitoring, dependency cascades, continuous learning, prioritisation engine, code indexing tree-sitter, self-evaluation tests lint coverage, dashboard cost estimation."
  },
  {
    slug: "cli-reference",
    title: "CLI commands",
    description: "Complete command reference for the Claudear CLI.",
    headings: [
      { id: "all-commands", text: "All commands", level: 2 },
      { id: "command-reference", text: "Command reference", level: 2 },
      { id: "claudear-start", text: "claudear start", level: 3 },
      { id: "claudear-stop", text: "claudear stop", level: 3 },
      { id: "claudear-status", text: "claudear status", level: 3 },
      { id: "claudear-pause", text: "claudear pause", level: 3 },
      { id: "claudear-resume", text: "claudear resume", level: 3 },
      { id: "claudear-activity", text: "claudear activity", level: 3 },
      { id: "claudear-seed", text: "claudear seed", level: 3 },
      { id: "claudear-dry-run", text: "claudear dry-run", level: 3 },
      { id: "claudear-poll", text: "claudear poll", level: 3 },
      { id: "claudear-webhook", text: "claudear webhook", level: 3 },
      { id: "claudear-trigger", text: "claudear trigger", level: 3 },
      { id: "claudear-reset", text: "claudear reset", level: 3 },
      { id: "claudear-stats", text: "claudear stats", level: 3 },
      { id: "claudear-sources", text: "claudear sources", level: 3 },
      { id: "claudear-dashboard", text: "claudear dashboard", level: 3 },
      { id: "claudear-repos", text: "claudear repos", level: 3 },
      { id: "claudear-prs", text: "claudear prs", level: 3 },
      { id: "claudear-retries", text: "claudear retries", level: 3 },
      { id: "claudear-inference", text: "claudear inference", level: 3 },
      { id: "claudear-report", text: "claudear report", level: 3 },
      { id: "claudear-diag", text: "claudear diag", level: 3 },
      { id: "claudear-users", text: "claudear users", level: 3 }
    ],
    text: "Complete CLI reference. claudear start: start watcher daemon with --port --poll --poll-interval --no-webhooks --no-dashboard. claudear stop: stop running daemon. claudear status: show daemon status. claudear pause: pause watcher stops processing new issues. claudear resume: resume paused watcher. claudear activity: show recent activity with optional limit. claudear seed: mark all existing issues as seen. claudear dry-run: show what would be processed without running Claude. claudear poll: foreground polling with optional interval and port. claudear webhook: start webhook server with --setup-webhooks --base-url for auto-registration. claudear trigger SOURCE ISSUE_ID: manually trigger a fix. claudear reset SOURCE ISSUE_ID: reset failed attempt for retry. claudear stats: show fix attempt statistics. claudear sources: list configured sources. claudear dashboard: start dashboard API with optional port and --dashboard-dir. claudear repos: list index search stats link graph cascade discover sync. claudear prs: list and monitor with --continuous. claudear retries: list and process. claudear inference: stats history feedback with --correct --actual-repo. claudear report: preview send schedule with --daily --weekly --hour. claudear diag: db release-graph release-check release-path. claudear users: seed admin user with --email --password --name. Global options: --config path, --log-dir directory, --help, --version."
  },
  {
    slug: "development",
    title: "Development",
    description: "Local setup, repo layout, build/test workflows, and contributing guide.",
    headings: [
      { id: "prerequisites", text: "Prerequisites", level: 2 },
      { id: "build-commands", text: "Build commands", level: 2 },
      { id: "test-commands", text: "Test commands", level: 2 },
      { id: "production-e2e-smoke-test-env-vars", text: "Production E2E smoke test env vars", level: 3 },
      { id: "dashboard-frontend-development", text: "Dashboard frontend development", level: 2 },
      { id: "high-signal-repository-layout", text: "High-signal repository layout", level: 2 },
      { id: "runtime-and-backend", text: "Runtime and backend", level: 3 },
      { id: "frontend-and-website", text: "Frontend and website", level: 3 },
      { id: "e2e-and-scripts", text: "E2E and scripts", level: 3 },
      { id: "prompt-customization-and-conventions", text: "Prompt customization and conventions", level: 2 },
      { id: "practical-debugging-workflow-for-contributors", text: "Practical debugging workflow", level: 2 },
      { id: "packaging-and-release-sensitive-changes", text: "Packaging and release-sensitive changes", level: 2 },
      { id: "regenerating-docs-after-changes", text: "Regenerating docs after changes", level: 2 }
    ],
    text: "Prerequisites: Rust 1.93+ Bun dashboard tooling Docker optional. Build commands: make build debug, make build-release with embedded dashboard, make install to /usr/local/bin. Test commands: make test Rust tests, make test-all Rust and dashboard, make test-prod-e2e production E2E smoke requires credentials, make check format lint test. Production E2E smoke test requires Linear GitHub and agent credentials with dedicated test repo. Dashboard frontend development: make dashboard install deps, make dashboard-dev dev server on 5173, make dashboard-build production, make dashboard-test. Repository layout runtime and backend: src/main.rs CLI entry, src/config.rs configuration, src/watcher.rs orchestration core, src/source issue integrations, src/webhook server handlers, src/runner agent providers, src/repo discovery indexing, src/api dashboard API, src/notifier channels ask orchestration, src/regression src/release src/reports monitoring, src/learning src/feedback src/prioritisation, src/ipc daemon protocol. Frontend: dashboard React app, website landing page docs. E2E: src/bin/e2e scenarios, scripts/prod-e2e-smoke.sh, scripts/screenshots. Prompt customization: AGENT.md per-repo and config-level agent instructions. Debugging workflow: dry-run after config changes, foreground poll for debugging, dashboard API for analytics, activity and diag for runtime diagnostics. Packaging: binaries Docker images Homebrew APT. Regenerating docs: bun scripts/docs/generate-website-docs.ts then verify HTML and assets."
  }
];
