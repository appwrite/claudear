<p align="center">
  <h1 align="center">Claudear</h1>
  <p align="center">
    <strong>Autonomous issue-to-PR pipeline powered by Claude Code</strong>
  </p>
  <p align="center">
    <a href="https://github.com/abnegate/claudear/actions/workflows/ci.yml"><img src="https://github.com/abnegate/claudear/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://github.com/abnegate/claudear/releases"><img src="https://img.shields.io/github/v/release/abnegate/claudear" alt="Release"></a>
    <a href="https://github.com/abnegate/claudear/blob/main/LICENSE"><img src="https://img.shields.io/github/license/abnegate/claudear" alt="License"></a>
    <a href="https://github.com/abnegate/claudear"><img src="https://img.shields.io/badge/rust-1.98+-orange.svg" alt="Rust"></a>
  </p>
</p>

Claudear watches your issue trackers and error monitoring services, automatically spawning [Claude Code](https://docs.anthropic.com/en/docs/claude-code) agents to fix issues and open pull requests: no human in the loop required (unless Claude has a question for you).

Point it at Linear, Sentry, Jira, GitLab, Discord, Slack, or GitHub review comments. It figures out which repo the issue belongs to, clones it, runs Claude Code (or Codex) with your project's conventions, opens a PR, monitors the PR through merge, auto-resolves the source issue, and learns from the outcome to get smarter over time.

---

## Table of Contents

- [How It Works](#how-it-works)
- [Features](#features)
- [Architecture](#architecture)
- [Installation](#installation)
- [Quick Start](#quick-start)
- [Configuration](#configuration)
- [Usage](#usage)
  - [Daemon Mode](#daemon-mode)
  - [Polling Mode](#polling-mode-foreground)
  - [Webhook Mode](#webhook-mode)
  - [Graceful Shutdown](#graceful-shutdown)
  - [Manual Triggers](#manual-triggers)
  - [PR Management](#pr-management)
  - [PR Distribution](#pr-distribution-1)
  - [Retry Management](#retry-management)
  - [Dashboard](#dashboard)
  - [Scheduled Reports](#scheduled-reports)
  - [Multi-Repository Cascading](#multi-repository-cascading)
  - [Human Q&A Loop](#human-qa-loop)
  - [Inference Analytics](#inference-analytics)
  - [Release Tracking](#release-tracking)
  - [Deploy QA](#deploy-qa)
  - [Diagnostics](#diagnostics)
  - [Code Chat](#code-chat)
  - [Dry Run](#dry-run)
  - [User Registry](#user-registry)
- [Fix Attempt Lifecycle](#fix-attempt-lifecycle)
- [AI Feedback Loop](#ai-feedback-loop)
  - [Embedding-Based Learning](#embedding-based-learning)
  - [Continuous Learning Pipeline](#continuous-learning-pipeline)
- [Custom Prompt Templates](#custom-prompt-templates)
  - [A/B Experiments](#ab-experiments)
- [Running as a Service](#running-as-a-service)
- [Docker](#docker)
- [CI/CD](#cicd)
- [Development](#development)
- [License](#license)

---

## How It Works

```
 Linear   Sentry   Jira   GitLab   Discord   Slack   Telegram   WhatsApp   GitHub Reviews
   |        |       |       |         |        |        |          |             |
   +--------+-------+-------+---------+--------+--------+----------+-------------+
                                      |
                                      v
          ┌──────────────────────────────────────────────────────────────────────────────┐
          |                              Claudear Watcher                                |
          |                                                                              |
          |   1. Detect new issue (poll or webhook)                                      |
          |   2. Prioritise by severity, frequency, blast radius, and clustering         |
          |   3. Infer target repository from stack traces, file paths, context          |
          |   4. Clone repo, spawn Claude Code / Codex agent with project conventions    |
          |   5. Self-evaluate the fix (tests, lint, static analysis, coverage)          |
          |   6. Create a PR, notify you (Discord / Slack / Email / SMS / Push / ...)    |
          |   7. Monitor PR through merge, auto-resolve issue on source                  |
          |   8. Watch for regressions for 24 hours                                      |
          |   9. Extract learnings from outcomes, diffs, and reviews                     |
          |  10. Cascade fixes through downstream dependency repos                       |
          └──────────────────────────────────────────────────────────────────────────────┘
```

---

## Features

### Issue Sources
- **Linear**: Trigger on labels (`auto-implement`, `claude`) or states (`backlog`, `todo`), with team/project filtering and assignee triggers
- **Sentry**: Process top escalating errors by event count, time period, and escalation threshold
- **Jira**: JQL-based issue filtering with label, status, issue type, and assignee support (Cloud + Server/DC)
- **GitHub Issues**: Monitor issues across repositories with label and state triggers
- **GitLab**: Fetch issues from GitLab groups and projects with label and state triggers
- **Discord**: Process messages and threads from Discord channels as issues
- **Slack**: Poll Slack channels for messages as issues
- **WhatsApp**: Receive WhatsApp messages via Cloud API webhooks
- **Telegram**: Poll Telegram chats via Bot API
- **GitHub Review Comments**: Respond to PR review comments tagged with `@claudear`, with `allowed_bots` to process comments from specific bots (e.g., Copilot)
- **GitLab MR Comments**: Respond to MR review comments tagged with `@claudear`, with `allowed_bots` support
- Per-source rate limiting, concurrent processing controls, and configurable poll intervals

### Intelligent Repository Routing
- Automatically determines the target repository from stack traces, file paths, and issue content
- Confidence scoring ranks repository matches so fixes go to the right place
- Scans configured GitHub organizations and local paths for repo discovery
- Full-text file index across all repositories for fast context lookups

### Autonomous Fix Pipeline
- Spawns real Claude Code or Codex processes with full tool access (read, edit, bash, etc.)
- Multi-provider support: Claude Code (primary), OpenAI Codex, with Gemini and Copilot planned
- A/B experiment infrastructure: test providers against each other with weighted routing
- Configurable model selection: Sonnet, Opus, Haiku, or any model ID
- Separate `classification_model` for repo routing (use a cheaper model like Haiku for classification)
- Project-specific `AGENT.md` files customize coding conventions per repo
- Global instructions and tool permissions via `claudear.toml`
- Configurable execution timeout (default 6 hours)

### Multi-Repository Cascading
- Tracks dependency graphs between repositories (NPM, Python, etc.)
- Automatically propagates fixes through dependent repos using BFS traversal
- Configurable cascade depth limits

### Human Q&A Loop
- When Claude is blocked on ambiguity, it asks a question via your notification channels
- Claudear fans out the question to all enabled notifiers (Discord, Slack, Email, etc.)
- Reply-capable channels: Discord, Slack, and Email — first reply wins
- Q&A pairs are stored and reused via embedding-based semantic matching so the same question is never asked twice
- Scoped matching (source+repo at 0.82 threshold) with global fallback (0.88 threshold)
- Configurable timeouts with best-effort continuation

### AI Feedback Loop
- Tracks outcomes of every fix attempt (merged, closed, failed)
- Generates local vector embeddings for all issue content (no external APIs: runs on ONNX Runtime)
- Finds similar past issues and extracts patterns from successes and failures
- Enhances future prompts with learnings from past outcomes
- Supported models: Nomic, MiniLM, BGE

### Prioritisation Engine
- Composite severity scoring from multiple signals: severity (30%), frequency (25%), regression risk (20%), blast radius (15%), clustering (10%)
- Blast radius classification: 5 tiers from critical (auth/payment) to cosmetic (docs)
- Content clustering: groups similar issues by error type, culprit, and title similarity
- Suppression rules: skip known-noisy issues with pattern matching (contains, regex) before they consume processing slots
- Configurable weights and path patterns per tier

### Self-Evaluation
- Before/after comparison of tests, lint, static analysis, and code coverage
- Auto-detects project tooling or accepts custom commands
- Posts evaluation results as PR comments
- Optionally fails fix attempts on quality regressions
- Per-tool and total timeout configuration

### Continuous Learning
- Extracts learnings from Claude's execution logs automatically
- Analyzes PR diffs on merge to extract coding patterns
- Promotes repeated Q&A answers to standing instructions (configurable threshold)
- Accumulates per-repository knowledge from successful fixes
- Classifies reviewer feedback patterns and promotes common fixes
- Tracks Claude's problem-solving strategies per issue type
- Scores fix quality based on merge velocity
- Detects clusters of correlated issues within configurable time windows
- Cross-repo failure correlation detection
- Optionally auto-generates `AGENT.md` from accumulated knowledge

### Code Indexing
- Tree-sitter based parsing for 12+ languages (Rust, JS, TS, Python, Go, Java, C, C++, Ruby, PHP, Swift, Kotlin)
- Semantic search across all repositories via embeddings
- Configurable file size limits and batch sizes

### PR Lifecycle Management
- Monitors PRs through merge/close with configurable poll intervals
- Automatically resolves source issues (Linear/Sentry) when PRs merge
- Exponential backoff retries for failed or closed PRs (configurable max retries)
- Full lifecycle tracking: Pending -> Success -> Merged -> Resolved (with retry paths)

### PR Distribution
- Makes one developer responsible for each open Claudear PR and pings them on Discord or Slack as soon as it opens
- Scores ownership by who asked for the work, CODEOWNERS, and recent commits, then balances so nobody holds more than their fair share
- Sticky assignments persisted in SQLite, plus a scheduled digest of the PRs still waiting on each assignee

### Regression Monitoring
- Hourly checks for 24 hours after a fix deploys
- Detects re-opened issues and error rate spikes on both Linear and Sentry
- Configurable similarity thresholds and event count minimums

### Release Tracking
- Dependency-aware release detection across repository graphs
- Tracks when fixes land in production through dependency paths
- Semantic versioning support

### Deploy QA
- Durable last-seen watches for GitHub release tips (separate from regression inclusion tracking)
- Tag filters (`any`, `suffix:-db`, `not_suffix:-db`, `prefix:apps/console/`) per track, so tracks can split one repo's tags, including a monorepo's app-namespaced bare tags
- Live QA agent enqueue — runs real checks against the release from a private per-run directory, and is told never to open fix PRs for a release announcement
- Discord `#releases` outcome from the report: verified or unverified reply (no @), FAIL thread + `@releaser` from the user registry (`github-discord-map.json` as fallback); a post of its own when nothing announced the tip

### Notifications
- **Discord**: Webhook messages + bot reply polling for Q&A, rich embeds, thread tracking
- **Slack**: Bot API messages + reply polling for Q&A, webhook alternative
- **Email**: SMTP sending + IMAP reply polling for Q&A
- **SMS**: Twilio integration
- **Push**: Pushover notifications with priority levels
- **WhatsApp**: WhatsApp Business Cloud API
- **Telegram**: Telegram Bot API with HTML formatting
- **Console**: Always-on logging
- Mix and match any combination of channels

### User Registry
- Map team members across Linear, GitHub, Sentry, Jira, GitLab, Discord, Slack, Email, SMS, Push, WhatsApp, Telegram
- Route notifications to the person assigned to the issue
- Per-user notification channel preferences
- Everyone edits their own identities in the dashboard; admins edit anyone's, invite people who have no login yet, and bulk-load the directory over the API
- Edits apply live, with no restart; `[users]` in config stays as seed and fallback

### Analytics Dashboard
- Live web UI (React + TypeScript + Tailwind)
- Stats overview: total attempts, success rate, merge rate, cost estimates
- Status breakdown, source-by-source metrics, recent attempt history
- PR analytics: review metrics, merge rates, rejection analysis
- Cost estimation with configurable engineer hourly rate for ROI calculation
- Retryable issues view with one-click retry
- Embedded in the release binary: no separate deployment needed

### Scheduled Reports
- Daily, weekly, and monthly automated status reports
- Breakdown of attempts, success/failure rates, PR metrics, pending work
- Delivered via all configured notification channels

### Daemon Mode & IPC
- Runs as a background service with full IPC control
- `start` / `stop` / `pause` / `resume` / `status` / `activity` commands
- Unix socket communication with configurable timeout
- Graceful shutdown: stops taking new work and gives in-flight runs up to 30 seconds to finish

### Security
- API security headers (HSTS, X-Content-Type-Options, X-Frame-Options)
- CSRF protection with double-submit cookie pattern
- Extended rate limiting per endpoint
- Idle session timeout with automatic cleanup
- Password hashing (bcrypt) and encrypted session tokens

### Webhooks
- Real-time event processing from all configured sources (Linear, Sentry, GitHub, GitLab, Jira, Slack, Discord, Telegram, WhatsApp)
- HMAC-SHA256 signature verification
- One-command auto-configuration: `claudear webhook --setup --base-url <url>`

### Integrations at a Glance

| Category | Integration | Protocol | Notes |
|----------|------------|----------|-------|
| **Issue Sources** | Linear | REST API | Labels, states, team/project filters |
| | Sentry | REST API | Escalating errors, event thresholds |
| | Jira | REST API | JQL queries, labels, status filters (Cloud + Server/DC) |
| | GitHub Issues | REST API | Cross-repo monitoring |
| | GitLab | REST API | Group and project issues |
| | Discord | Bot API | Channel messages and threads |
| | Slack | Bot API | Channel message polling |
| | WhatsApp | Cloud API | Webhook-fed message buffer |
| | Telegram | Bot API | `getUpdates` long-polling |
| **SCM** | GitHub | REST + Git | PRs, review comments, webhooks, App auth, bot comment processing |
| | GitLab | REST + Git | MRs, issue resolution, review comments, bot comment processing |
| **Notifications** | Console | stdout | Always-on logging |
| | Discord | Webhook + Bot | Rich embeds, reply-based Q&A, thread tracking |
| | Slack | Bot API + Webhook | Messages, reply-based Q&A |
| | Email | SMTP + IMAP | Send + reply polling for Q&A |
| | SMS | Twilio API | Text message alerts |
| | Push | Pushover API | Mobile push notifications |
| | WhatsApp | Cloud API | WhatsApp Business messages |
| | Telegram | Bot API | Chat messages with HTML formatting |
| **Agent Runners** | Claude Code | CLI | Primary runner, full tool access |
| | Codex | CLI | OpenAI Codex runner |
| | Gemini | CLI | Google Gemini *(planned)* |
| | Copilot | CLI | GitHub Copilot *(planned)* |
| **Storage** | SQLite | Local file | WAL mode, Vectorlite extension |
| **Embeddings** | Nomic | ONNX | Default model, local inference |
| | MiniLM | ONNX | Lightweight alternative |
| | BGE | ONNX | BAAI general embedding |
| **Code Indexing** | Tree-sitter | Local | 12+ languages, semantic search |

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                                  Claudear                                    │
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  ┌────────┐ ┌────────┐ ┌──────┐ ┌────────┐ ┌─────────┐ ┌───────┐          │
│  │ Linear │ │ Sentry │ │ Jira │ │ GitLab │ │ Discord │ │ Slack │ ← Sources│
│  └───┬────┘ └───┬────┘ └──┬───┘ └───┬────┘ └────┬────┘ └──┬────┘          │
│      │          │         │         │           │         │                │
│      └────┬─────┴────┬────┴────┬────┴─────┬─────┴────┬────┘                │
│           │          │         │          │          │                      │
│           ▼          ▼         ▼          ▼          ▼                      │
│    ┌──────────────────────────────────────────────────────┐                 │
│    │                    Watcher                           │                 │
│    │  (polls, webhooks, prioritises, matches, rate-limits)│                 │
│    └───────────────────────┬──────────────────────────────┘                 │
│                            │                                                │
│    ┌───────────┬───────────┼──────────┬──────────┬───────────┐             │
│    │           │           │          │          │           │             │
│    ▼           ▼           ▼          ▼          ▼           ▼             │
│ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌──────────┐     │
│ │Prioriti│ │  Agent │ │Notifier│ │   PR   │ │  Self  │ │  Repo    │     │
│ │-sation │ │ Runner │ │(Discord│ │Monitor │ │  Eval  │ │  Index   │     │
│ │ Engine │ │(Claude,│ │ Slack, │ │        │ │(tests, │ │(tree-sit,│     │
│ │        │ │ Codex) │ │Email..)│ │        │ │ lint)  │ │inference)│     │
│ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └──────────┘     │
│                                                                            │
│ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌──────────┐     │
│ │Learning│ │Regress-│ │Release │ │Cascade │ │  IPC   │ │ SQLite + │     │
│ │ System │ │  ion   │ │Tracker │ │ Engine │ │(daemon)│ │Vectorlite│     │
│ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └──────────┘     │
│                                                                            │
│ ┌──────────────────────────────────────────────────────────────────────┐   │
│ │                     Dashboard (React + Tailwind)                     │   │
│ │  Stats · Analytics · PRs · Attempts · Retries · Reports · Config    │   │
│ └──────────────────────────────────────────────────────────────────────┘   │
│                                                                            │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

## Installation

### Homebrew (macOS/Linux)

```bash
brew tap abnegate/tap
brew install claudear
```

### APT (Debian/Ubuntu)

```bash
curl -fsSL https://abnegate.github.io/apt-repo/pubkey.gpg | sudo gpg --dearmor -o /usr/share/keyrings/claudear.gpg
echo "deb [signed-by=/usr/share/keyrings/claudear.gpg] https://abnegate.github.io/apt-repo stable main" | sudo tee /etc/apt/sources.list.d/claudear.list
sudo apt update && sudo apt install claudear
```

### Pre-built Binaries

Download from the [releases page](https://github.com/abnegate/claudear/releases) (Linux, macOS Intel/ARM).

### From Source

```bash
git clone https://github.com/abnegate/claudear.git
cd claudear
cargo build --release
# Binary at target/release/claudear
```

### Docker

```bash
docker pull ghcr.io/abnegate/claudear:latest
```

---

## Quick Start

```bash
# 1. Create your config file
cp claudear.example.toml claudear.toml
# Edit claudear.toml with your API keys (issues.linear.api_key, scm.github.token, etc.)

# 2. Seed existing issues (so Claudear doesn't process old issues)
claudear seed

# 3. Start watching (daemon mode with polling + webhooks + dashboard)
claudear start --poll --port 3100

# 4. Open the dashboard
open http://localhost:3100
```

That's it. Label a Linear issue with `auto-implement` or `claude`, and Claudear will pick it up, fix it, and open a PR.

---

## Configuration

Claudear uses a TOML configuration file. Copy the example and customize:

```bash
cp claudear.example.toml claudear.toml
```

By default, Claudear looks for `claudear.toml` in the current directory. Override with:

```bash
claudear --config /path/to/config.toml poll
```

### Environment Variable Overrides

All config values can be overridden with environment variables, useful for keeping secrets out of version control and for container deployments:

| Variable | Config Path |
|----------|-------------|
| `CLAUDEAR_LINEAR_API_KEY` | `issues.linear.api_key` |
| `CLAUDEAR_SENTRY_AUTH_TOKEN` | `issues.sentry.auth_token` |
| `CLAUDEAR_GITHUB_TOKEN` | `scm.github.token` |
| `CLAUDEAR_GITLAB_TOKEN` | `scm.gitlab.token` |
| `CLAUDEAR_JIRA_API_TOKEN` | `issues.jira.api_token` |
| `CLAUDEAR_LINEAR_WEBHOOK_SECRET` | `issues.linear.webhook_secret` |
| `CLAUDEAR_SENTRY_CLIENT_SECRET` | `issues.sentry.client_secret` |
| `CLAUDEAR_GITHUB_WEBHOOK_SECRET` | `scm.github.webhook_secret` |
| `CLAUDEAR_GITLAB_WEBHOOK_SECRET` | `scm.gitlab.webhook_secret` |
| `CLAUDEAR_EMBEDDING_MODEL` | Embedding model (`nomic`, `minilm`, `bge`) |
| `CLAUDEAR_EMBEDDING_CACHE_DIR` | Embedding model cache directory |
| `CLAUDEAR_SENTRY_DSN` | Sentry DSN for backend error reporting |
| `CLAUDEAR_SENTRY_RELEASE` | Sentry release tag |
| `CLAUDEAR_SENTRY_ENVIRONMENT` | Sentry environment name |
| `CLAUDEAR_TELEGRAM_WEBHOOK_SECRET` | Telegram webhook secret token |
| `CLAUDEAR_WHATSAPP_WEBHOOK_VERIFY_TOKEN` | WhatsApp webhook verify token |
| `CLAUDEAR_VECTORLITE_PATH` | Path to vectorlite SQLite extension |
| `CLAUDEAR_TLS_ENABLED` | `tls.enabled` |
| `CLAUDEAR_TLS_DOMAINS` | `tls.domains` (comma-separated) |
| `CLAUDEAR_TLS_EMAIL` | `tls.email` |
| `CLAUDEAR_TLS_PRODUCTION` | `tls.production` |
| `CLAUDEAR_TLS_CACHE_DIR` | `tls.cache_dir` |
| `CLAUDEAR_TLS_HTTPS_PORT` | `tls.https_port` |
| `CLAUDEAR_TLS_HTTP_REDIRECT_PORT` | `tls.http_redirect_port` |
| `CLAUDEAR_DISCORD_BOT_TOKEN` | `notifiers.discord.bot_token` |
| `CLAUDEAR_SLACK_BOT_TOKEN` | `notifiers.slack.bot_token` |
| `CLAUDEAR_DASHBOARD_PUBLIC_URL` | `dashboard.public_url` (empty means unset) |

### Minimal Configuration

```toml
workspace = "~/.claudear/repos"

known_orgs = ["your-github-org"]

auto_discover_paths = ["~/projects"]

[issues.linear]
api_key = "lin_api_xxxx"
```

### Configuration Reference

| Section | Description |
|---------|-------------|
| `workspace` | Directory where repositories are cloned **(required)** |
| `known_orgs` | GitHub organizations to track for auto-discovery |
| `auto_discover_paths` | Local paths to scan for repository clones |
| `poll_interval_ms` | Polling interval in milliseconds (default: 300000 = 5 min) |
| `db_path` | SQLite database path (default: `./claudear.db`) |
| `webhook_port` | Webhook server port (default: 3100) |
| `bind_address` | HTTP server bind address (default: `127.0.0.1`, use `0.0.0.0` for Docker) |
| `max_issues_per_cycle` | Max issues per poll cycle (default: 5) |
| `max_concurrent` | Max concurrent issue processing (default: 1) |
| `processing_delay_ms` | Delay between processing issues in ms (default: 5000) |
| `agent` | Agent provider config, model, instructions, permissions, A/B experiments (see [Custom Prompt Templates](#custom-prompt-templates)) |
| `issues.linear` | Linear API key, trigger labels/states/assignees, team/project filters, rate limits |
| `issues.sentry` | Sentry auth token, org slug, project filters, escalation thresholds |
| `issues.jira` | Jira base URL, auth, project keys, JQL filters, issue types |
| `issues.discord` | Discord bot token, listen channel ID |
| `issues.slack` | Slack bot token, listen channel ID |
| `scm.github` | GitHub token, PR poll interval, auto-resolve on merge, review trigger tag, allowed bots |
| `scm.github.app` | GitHub App authentication (App ID, private key, installation ID) |
| `scm.gitlab` | GitLab token, base URL, groups, MR poll interval, review trigger tag, allowed bots |
| `notifiers.discord` | Discord webhook URL, bot token, channel ID for Q&A |
| `notifiers.slack` | Slack bot token, channel ID, webhook URL for Q&A |
| `notifiers.email` | SMTP sending + IMAP reply polling |
| `notifiers.sms` | Twilio account SID, auth token, phone numbers |
| `notifiers.push` | Pushover API token, user key, priority |
| `notifiers.whatsapp` | WhatsApp phone number ID, access token, recipient numbers |
| `notifiers.telegram` | Telegram bot token, chat ID, recipient chat IDs |
| `ask` | Human Q&A loop: timeout, poll interval, max rounds, semantic thresholds |
| `retry` | Max retries, base delay, max delay (exponential backoff) |
| `regression` | Check interval, monitoring duration, event thresholds |
| `deploy_qa` | Live release-tip watches (tracks, tag filters, Discord `#releases`) |
| `pr_distribution` | Assign each open Claudear PR to one developer, ping on open, scheduled per-assignee digest |
| `cascade` | Enable/disable cascading, max depth, per-dependency rules |
| `learning` | Continuous learning: log extraction, diff analysis, Q&A promotion, repo knowledge |
| `prioritisation` | Composite scoring weights, blast radius paths, clustering, suppression rules |
| `code_index` | Tree-sitter code indexing: enable, file size limits, batch size |
| `evaluation` | Self-evaluation: test/lint/coverage deltas, timeouts, custom commands |
| `dashboard` | Dashboard display: cost estimation, engineer hourly rate, public URL for absolute invite links |
| `tls` | Let's Encrypt TLS auto-provisioning: domains, ACME settings, HTTPS/redirect ports |
| `users` | User registry mapping across services; seed and fallback for dashboard identities (see [User Registry](#user-registry-1)) |

See [`claudear.example.toml`](claudear.example.toml) for a fully documented example with every option.

---

## Usage

### Daemon Mode

The recommended way to run Claudear in production. Starts a background daemon with IPC control.

```bash
# Start with polling, webhooks, and dashboard
claudear start --poll --port 3100

# Start with custom polling interval
claudear start --poll --poll-interval 60000

# Start without webhooks or dashboard
claudear start --poll --no-webhooks --no-dashboard

# Stay in the foreground (for systemd, launchd, or Docker)
claudear start --poll --foreground

# Control the daemon
claudear status              # Check daemon health
claudear pause               # Stop picking up new issues
claudear resume              # Resume processing
claudear activity            # View recent activity
claudear activity 50         # Show last 50 entries
claudear stop                # Stop after in-flight runs finish (up to 30s)
```

### Polling Mode (Foreground)

```bash
# Poll all enabled sources (default 5 minute interval)
claudear poll

# Custom interval
claudear poll 60000

# With dashboard
claudear poll --port 8080
```

### Webhook Mode

Real-time event processing via webhooks from all configured sources.

```bash
# Start webhook server
claudear webhook

# Auto-configure webhooks with Linear/Sentry APIs
claudear webhook --setup --base-url https://my-server.example.com:3100
```

The `--setup` flag:
1. Connects to Linear/Sentry APIs using your configured API keys
2. Creates webhooks pointing to your server
3. Retrieves signing secrets and writes them to your `.env` file
4. Starts the webhook server with verification enabled

### Graceful Shutdown

`claudear start`, `claudear poll`, `claudear webhook`, and `claudear dry-run` shut down on SIGTERM, SIGINT, or SIGHUP, as `kill`, `systemctl stop`, `docker stop`, Ctrl+C, or a closing terminal send them. A daemon started with `claudear start` also shuts down on `claudear stop`; stop the other commands with a signal. Any of these signals that was already ignored when Claudear started stays ignored: a Claudear started under `nohup` keeps running when its terminal closes, and one that a script starts in the background keeps running on Ctrl+C, because the script's shell ignores SIGINT for it. Claudear then:

1. Stops taking new work:
   - polling and background housekeeping start no new runs
   - issue webhooks are answered with `503 Service Unavailable` and `Retry-After: 60`, and not recorded, so a redelivery is processed once Claudear is back (GitHub review and pull request webhooks are still accepted)
   - IPC `Trigger` and `ProcessRetries` commands are refused
2. On a signal, interrupts the agent CLIs at once, logging `Interrupting the agent CLIs of the runs in flight`. Each agent CLI leads a process group of its own, out of reach of the terminal's Ctrl+C, so Claudear passes on the SIGINT that Ctrl+C would send. It does not wait for the runs first: service managers and Docker follow SIGTERM with SIGKILL after a deadline, and agent runs take minutes. `claudear stop`, or a service that stops on its own, leaves the agent CLIs running instead.
3. Waits up to 30 seconds for in-flight runs to finish, as interrupted runs do once they have recorded how they ended. A run that records a failure is retried later, like any failed run.
4. Interrupts the agent CLIs still running after that, logging `Interrupting the agent CLIs of the runs still in flight after 30s`, and gives their runs up to 5 more seconds to record how they ended. Any agent process still running then is killed. Runs that background housekeeping started (retries, review runs, merge follow-ups, and release cascades) stop when the 30 seconds run out instead, and their agent CLIs are killed straight away.
5. Exits, logging `Exiting with runs still in flight` if any are left. Blocking work, such as a local model call, gets up to 5 more seconds to stop before the process ends. A run that could not record how it ended leaves its fix attempt `pending` until a running daemon (`claudear start`, `claudear poll` or `claudear webhook`) finds that the run has sent no heartbeat for 5 minutes. Then an attempt started in the last 3 days goes back up for retry without spending a retry, and an older one is closed as `cannot_fix`, which `claudear reset <source> <issue_id>` undoes. Cascade attempts stay `pending`.

While it drains, `claudear status` reports `Running: false`. `claudear stop` waits up to 45 seconds for the daemon to exit and reports progress every 5 seconds. It prints `Daemon stopped.` and exits with code 0 once the daemon has exited, or exits with code 1 if the daemon is still running after 45 seconds. Until the daemon has exited, `claudear start` cannot start another one: the new daemon exits at once with `Another claudear daemon is already running. Stop it first with 'claudear stop'`. A signal that reaches Claudear while it shuts down, including after `claudear stop`, forces the exit: Claudear kills every agent CLI and dies of SIGINT at once.

Exit codes:

- `0`: graceful shutdown, including when the drain runs out of time
- `130` (death by SIGINT): a signal during the shutdown forced the exit
- `1`: a service failed or stopped on its own (`The <name> service stopped unexpectedly`), which started the shutdown

One-shot commands that run agents (`claudear trigger`, `claudear action`, and `claudear retries process`) also stop on SIGTERM, SIGINT, or SIGHUP: they interrupt their agent CLIs at once, kill any still running 5 seconds later, and give the command up to 30 seconds to record how its runs ended before it exits as interrupted, dying of SIGINT. A second signal exits at once.

### Manual Triggers

```bash
# Trigger a fix for a specific issue
claudear trigger linear abc123-def456
claudear trigger sentry 12345678

# Reset a failed attempt for retry
claudear reset sentry 12345678

# View statistics
claudear stats

# List configured sources
claudear sources
```

### PR Management

```bash
# List all tracked PRs
claudear prs list

# Check pending PRs once
claudear prs monitor

# Run continuously
claudear prs monitor --continuous
```

### PR Distribution

Every open Claudear PR gets one developer responsible for getting it over the line. Developers come from the [user registry](#user-registry-1): everyone, console identities and `[users]` entries alike, who owns a GitHub login (a login two people list belongs to its registry owner), minus `excluded`. Mentions use the registry's Discord and Slack ids; the `[deploy_qa]` `github-discord-map.json` only fills in a missing Discord id and adds logins nobody in the registry owns. The roster is rebuilt for every PR and scheduled run, so console edits change assignments and mentions without a restart.

```toml
[pr_distribution]
enabled = true
digest_frequency = "daily"         # daily | weekly-monday .. weekly-sunday | monthly; anything else fails to load
digest_hour = 9                    # UTC
excluded = ["on-leave-login"]
tolerance = 0                      # PRs above the fair share an owner may hold
```

**Ownership.** Each roster developer is scored for a PR, strongest signal first:

| Signal | Points |
|--------|--------|
| Asked for the work: the issue's assignee as resolved by the user registry, a GitHub issue's assignee or author, or the Discord/Slack user who posted it | 100 for the closest, 100/n for the n-th |
| CODEOWNERS (`.github/`, root or `docs/`) names them for the changed files | 40 × share of changed files |
| Authored recent commits (last 30 per file) to the changed files | 20 × share of those commits |
| Authored recent commits (last 100) to the repository | 5 × share of those commits |

Team (`@org/team`) and email owners in CODEOWNERS are ignored.

**Mentions.** Only the assignee is ever pinged. PR titles are external text, so every `@` in them is broken with a zero-width space (and Slack's `<`, `>` and `&` are escaped), and Discord messages carry `allowed_mentions` limited to the assignee's user id.

**Balancing.** The fair share is the number of open PRs (already assigned plus waiting) divided by the roster size, rounded up, plus `tolerance`. PRs with the strongest ownership are placed first; each goes to its highest-scoring developer still under the fair share, otherwise to the least-loaded developer. Ties break by login, so the same inputs always give the same assignments.

**Lifecycle.** When Claudear opens a PR it is assigned at once and the assignee is pinged (`notify_on_open`). On the schedule, Claudear reconciles every open Claudear PR: assignments of merged or closed PRs are closed, PRs whose assignee left the roster are handed to someone else, unowned PRs are assigned, and each assignee gets a digest of their PRs that still need them. Assignments are stored in `pr_assignments` and stay with their assignee until one of those happens.

**Which PRs need someone.** A PR is listed (and an unowned one assigned) while it is open and not a draft, unless it is approved with auto-merge on, or its assignee has already reviewed its current head commit and has no outstanding review request. An approved PR without auto-merge is still listed: someone has to merge it.

### Retry Management

```bash
# List issues eligible for retry
claudear retries list

# Process all ready retries now
claudear retries process
```

### Dashboard

The dashboard is a React + TypeScript web UI that's embedded directly in the release binary.

```bash
# Start dashboard (default port 3100)
claudear dashboard

# Custom port
claudear dashboard 8080

# Serve from external build directory
claudear dashboard --dashboard-dir ./dashboard/dist
```

**What the dashboard shows:**
- Live statistics: total attempts, success rate, merge rate
- Status breakdown: pending, success, merged, closed, failed, cannot fix
- Source-by-source metrics (Linear, Sentry, Discord, GitHub)
- Recent attempt history with direct PR links
- Retryable issues with one-click retry
- Deploy QA: per-track release tips and their QA results, with admin-editable track config
- Integrations: every source, SCM, notifier and automation with its configured and enabled state

Console config edits are written to `claudear.toml` in normalised form and apply after restart; secrets are write-only; read-only mounts disable saving.

<p align="center">
  <img src="website/assets/screenshots/overview.png" alt="Dashboard overview" width="800" />
  <br />
  <em>Overview — live stats, source breakdown, and recent attempts at a glance</em>
</p>

<p align="center">
  <img src="website/assets/screenshots/analytics.png" alt="Analytics page" width="800" />
  <br />
  <em>Analytics — success rates by source, MTTR trend, cost estimates, and repo leaderboard</em>
</p>

<p align="center">
  <img src="website/assets/screenshots/prs.png" alt="Pull requests page" width="800" />
  <br />
  <em>Pull Requests — review metrics, merge rates, and rejection analysis</em>
</p>

**API endpoints:**

| Endpoint | Description |
|----------|-------------|
| `GET /api/health` | Health check |
| `GET /api/stats` | Statistics |
| `GET /api/stats/overview` | Full dashboard data |
| `GET /api/analytics/summary` | Analytics summary |
| `GET /api/attempts` | List attempts (with filtering) |
| `GET /api/attempts/:id` | Single attempt detail |
| `GET /api/attempts/:id/full` | Full attempt with execution log |
| `GET /api/sources` | Source information |
| `GET /api/retries` | Retryable issues |
| `POST /api/retries/:id/process` | Trigger retry |
| `GET /api/prs` | List tracked PRs |
| `GET /api/prs/analytics` | PR review analytics |
| `GET /api/activity` | Activity log |
| `GET /api/repos` | List repositories |
| `GET /api/repos/stats` | Repository statistics |
| `GET /api/repos/dependencies` | Dependency graph |
| `GET /api/inference/stats` | Inference success rates |
| `GET /api/regressions` | Regression checks |
| `GET /api/deploy-qa` | Deploy QA status per track |
| `GET /api/deploy-qa/tips` | Recent deploy QA tips (`?track=`) |
| `GET/PUT /api/deploy-qa/config` | Deploy QA config (admin) |
| `GET /api/features` | Integration and feature status |
| `GET /api/telemetry/overview` | Telemetry overview |
| `GET /api/telemetry/cost-analysis` | Cost analysis |

Users, identities, invites and the bulk directory load are covered in the [Identity API](#identity-api).

#### Reverse proxy

The chat websocket (`/api/chat/ws`) only accepts an upgrade whose `Origin` matches the host the browser connected to. Behind a reverse proxy, either preserve the original `Host` header or send it in `X-Forwarded-Host` or `Forwarded: host=`, otherwise every upgrade is rejected with 403. For nginx:

```nginx
location /api/chat/ws {
    proxy_pass http://127.0.0.1:3100;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
}
```

### Scheduled Reports

```bash
# Preview a report
claudear report preview daily
claudear report preview weekly

# Send a report immediately
claudear report send daily

# Start the scheduler
claudear report schedule --daily --hour 9
claudear report schedule --daily --weekly --hour 9
```

Reports include issues attempted/succeeded/failed, success rates, PRs created/merged/closed, source-by-source breakdown, and current pending/retryable issues. Delivered via all configured notification channels.

### Multi-Repository Cascading

Claudear tracks dependency graphs between repositories and can automatically propagate fixes through downstream repos.

```bash
# Discover repositories from configured paths
claudear repos discover
claudear repos discover --paths ~/projects ~/work --save

# List and manage indexed repos
claudear repos list
claudear repos index
claudear repos stats
claudear repos sync

# Search files across all repos
claudear repos search "auth middleware"

# Define and explore dependencies
claudear repos link my-lib my-app --dep-type npm
claudear repos graph
claudear repos graph --root my-lib

# Preview what a change would cascade to
claudear repos cascade my-lib
```

### Human Q&A Loop

When Claude encounters ambiguity that requires human input, it emits a structured question:

```json
{"question":"...","context":"...","options":["..."],"why":"..."}
```

**How it works:**
1. Question is fanned out to all enabled notification channels
2. Discord, Slack, and Email support reply polling (first reply wins)
3. Claude resumes immediately with the answer
4. Q&A pairs are stored and reused via semantic matching (source+repo scoped, then global fallback)
5. If timeout is reached and `ask.best_effort_on_timeout=true`, Claude continues with an explicit uncertainty note

**Requirements for reply channels:**
- **Discord**: `notifiers.discord.bot_token` + `notifiers.discord.channel_id`
- **Slack**: `notifiers.slack.bot_token` + `notifiers.slack.channel_id`
- **Email**: IMAP fields (`imap_host`, `imap_port`, `imap_username`, `imap_password`)

### Inference Analytics

Track how accurately Claudear routes issues to the correct repository.

```bash
# Success rates by confidence level
claudear inference stats

# Recent inference history
claudear inference history
claudear inference history --limit 50

# Provide feedback to improve future routing
claudear inference feedback 42 --correct
claudear inference feedback 43 --actual-repo my-other-repo
```

### Deploy QA

Durable driver for **new GitHub release tips** (Appwrite Labs cloud / edge / edge-db, and the console's tags in the `appwrite/appwrite` monorepo). Separate from `[regression]` release tracking, which watches **bug-fix inclusion** after a merge.

Works in every daemon mode — `claudear start` (no `--poll` needed), `claudear webhook` and `claudear poll`. After each release poll, pending tips are dispatched to the QA agent, at most `qa.max_concurrent` at a time, once the daemon has finished warm start. That dispatch is the only path that runs tips inside the daemon: the `--poll` source loop and `claudear seed` leave `deploy_qa` tips to it, and a tip another process started or finished in the meantime is skipped.

```toml
[deploy_qa]
enabled = true
poll_interval_ms = 300000
skip_if_previous_running = true
timeout_secs = 1800                # per live-QA run; the runner stops it past min(this, [agent] timeout_secs)
discord_channel_id = "990878183580651571"
github_discord_map_path = "/etc/claudear/github-discord-map.json"  # absolute: relative paths break under systemd

[[deploy_qa.tracks]]
name = "cloud"
repo = "appwrite-labs/cloud"
tag_filter = "any"                 # any | suffix:-db | not_suffix:-db | prefix:apps/console/

[[deploy_qa.tracks]]
name = "console"
repo = "appwrite/appwrite"
tag_filter = "prefix:apps/console/"
```

A track's newest tip is the first matching published release in GitHub's release order. A repo with no published releases falls back to its tags, newest by semantic version. A `prefix:` track always reads the tags under its prefix (`git/matching-refs`, so a monorepo's other tags never crowd them out), takes the highest semantic version after the prefix (`apps/console/1.2.141` beats `apps/console/1.2.99`), and skips prefixed tags that are not a version. It uses the tag's release when one is published and otherwise QAs the bare tag, giving the agent the compare range since the previous tag on the track and the version without its prefix. Tracks on one repo need filters that split its tags (`suffix:-db` / `not_suffix:-db`), or each runs QA on the same tag.

On a new tip Claudear:

1. Persists last-seen tag per track in SQLite (`deploy_qa_tips`) and skips duplicates. On first enable a track has no last-seen tag, so its current tip counts as new and gets one QA run.
2. Skips enqueue if a previous attempt on that track is still pending or running. A tip whose attempt ends without a verdict (agent error, timeout, or a report Claudear could not record) is marked `errored` so it no longer blocks the track. Tips left `running` longer than `[deploy_qa] timeout_secs` plus a margin of that timeout again (at least 10 minutes) are marked `errored` on the next poll tick, so a crashed or interrupted run stops blocking its track without affecting runs live in other processes that share the database (a `claudear trigger`, `claudear retries process` or second `claudear poll`). The retry manager may retry a failed attempt up to `[retry] max_retries` times, but only an `errored` tip is re-run: a tip that is running or has a recorded verdict is never re-run, by a retry or by `claudear trigger`.
3. Enqueues a synthetic `deploy_qa` issue (`track:repo:tag`) with the bundled playbook — **live QA that only reports**: the agent runs real checks against the release and is told never to open fix PRs, branches or commits (see **Guardrails** below). `claudear action` (`reply`, `verify` or `resolve`) refuses these issues.
4. Classifies the agent's report, records the tip as `verified`, `unverified` or `failed`, and posts the outcome under the release announcement in Discord `#releases` (the agent never posts itself):
   - **All verified** — the report ends in `DEPLOY_QA_VERDICT: ALL_VERIFIED` and no PR is `LIVE BLOCKED`: reply, no @.
   - **Unverified** — nothing failed, but a PR is `LIVE BLOCKED`, the footer is `DEPLOY_QA_VERDICT: UNVERIFIED`, or the footer is missing: reply, no @, so blocked checks never page the releaser but are never reported as verified either.
   - **FAIL** — any `LIVE FAIL` line or a `DEPLOY_QA_VERDICT: FAIL` footer: a thread under the announcement that `@`s the releaser (reusing the announcement's existing thread when it already has one). The releaser's GitHub login is looked up in the [user registry](#user-registry-1) first, console identities and `[users]` entries alike, so invited users count too; `github-discord-map.json` is consulted only when the person who owns that login in the registry has no `discord_id`, or no person owns it. A releaser found in neither gets no mention.
   - **No announcement found** — when none of the last 50 `#releases` messages announces the tip, the outcome is posted as a message of its own naming the repo and tag, so it is never dropped. This should be rare: a release whose announcement scrolled away, or a bare tag (no GitHub release, so the release feed never posts it). FAIL still `@`s the releaser when the tip has one; a bare tag has no release author, so it posts without the mention.

   Announcements are matched by the release URL first, so a release whose name differs from its tag (`Console 1.2.141` for `apps/console/1.2.141`) is still found, and otherwise by a title naming both the repo and the whole tag, where `console/0.3.0` never matches `apps/console/0.3.0`.

See [`playbooks/deploy_qa.md`](playbooks/deploy_qa.md) and [`github-discord-map.example.json`](github-discord-map.example.json). Live host probes are agent-driven; CI uses a no-op probe seam and mocked GitHub/Discord HTTP.

**Tool access.** A live-QA run gets the provider's fix-run access (`permissions`, and `skip_permissions` when set) plus its read-only tools (`readonly_tools`), and uses `qa_model` when one is set. A scoped rule allows only the commands it matches: with `permissions = ["Bash(git *)"]` the agent can run `git` but not `curl`, so HTTP checks need a rule such as `Bash(curl:*)` or `skip_permissions = true`. Unless `skip_permissions` is set, a run logs a warning when its allowed tools (`permissions` plus `readonly_tools`, so a `Bash` entry in either counts) grant no shell at all, and a different one when they grant only scoped `Bash(...)` rules.

**Where it runs.** Each run starts in a fresh directory of its own, `<workspace>/.claudear-deploy-qa/<short id>-<random>`, never a repo checkout, and on Unix the agent gets that directory's resolved path, with no symlinks in it. Claudear creates `.claudear-deploy-qa` and every run directory owner-only (0700 on Unix), and deletes a run's directory when the run ends, after any report is delivered; a run cut short by a daemon crash leaves its directory behind. On Unix the workspace must be owned by the daemon user and writable by no other user, or anyone who can write to it could swap `.claudear-deploy-qa` for a directory of their own. Group write is allowed only when the workspace's group is the daemon user's primary group, as with the user-private groups Ubuntu gives each user; if that group has other members, remove its write access. An existing `.claudear-deploy-qa` must be a real directory, not a symlink, and on Unix one the daemon user owns, which Claudear restricts to 0700 before use. Otherwise the run fails, as it also does when `.claudear-deploy-qa` is replaced while a run's directory is being created in it. The agent CLI gets `--strict-mcp-config` and `--setting-sources user`, so only the MCP servers Claudear attaches for `deploy_qa` load, and only your user-level Claude Code settings apply: never a `.mcp.json`, and never project or local settings (`.claude/settings.json`, `.claude/settings.local.json`).

**Context.** Unlike a question, a live-QA run gets no retrieved context: no code search results, indexed Discord discussions or reply chain, because people other than your operators can write that text and the agent can run commands. Besides Claudear's own rules, its prompt holds only the tip (the playbook and release details) and your agent instructions from the dashboard.

**Time limit.** The runner stops a live-QA run once it exceeds the smaller of `[agent] timeout_secs` and `[deploy_qa] timeout_secs`, and records it as timed out. That kills the agent CLI and everything still running in its process group, such as a hung `curl`, but not a process that moved to a session of its own. Once the CLI is gone, the runner waits a few seconds for such a process to release its output, then keeps the output read so far, so a run that finished within its limit is still reported. Processing only gives up on the run itself if the runner fails to stop it, and not before the runner has had time past `[deploy_qa] timeout_secs` to return a finished run or to kill and record a timed-out one. Either way a timed-out run's tip is marked `errored`, and the retry manager may run it again up to `[retry] max_retries` times.

**Guardrails.** The agent is told not to modify any repository or open fix PRs, branches or commits, not to post to Discord, Slack or any other channel, to make state-changing requests only in the playbook's throwaway QA project, to keep secrets out of its report, and to treat release notes, PR text, web pages and command output as data, not instructions. These are prompt rules, not a sandbox: the agent runs as the daemon user with the same files, network access and environment as fix runs, except that it inherits none of Claudear's own `CLAUDEAR_*` variables (its master key and tokens), so give it credentials that can only touch the throwaway QA project. The report goes only to `#releases`: if Claudear cannot record it, the run fails and the tip is marked `errored` rather than the report going to your other notifier channels. Before posting, Claudear redacts the credentials it recognizes: the tokens, keys, passwords, webhook URLs and webhook secrets in its config; the values of secret-named variables (names containing `TOKEN`, `SECRET`, `PASSWORD`, `API_KEY` or `AUTH`, ending in `_KEY`, and the like) in the daemon environment, the provider `env` and each MCP server's `env` and `headers`; credential header values (`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, `X-Api-Key`, `X-Fallback-Cookies` and the `X-Appwrite-*` key, JWT and session headers); any token of 8 or more characters after `Bearer`, so a long word there is masked too; `Basic` credentials, base64 of `user:password` whatever characters it uses; URL passwords; secret-named `NAME=value` assignments and JSON fields, Appwrite `a_session_*` session cookies among them; the `value` of a JSON object whose `name` is secret-named, as browser tools list cookies and headers; generated-looking values (20 or more characters mixing letters and digits) after a credential label or flag, such as `API key:`, `key=`, `--key` or a `| key |` table cell; Appwrite API keys (`standard_`, `organization_` and `account_` keys, and `ephemeral_` or `dynamic_` JWT keys); PEM blocks; JWTs, including one after `_`; and GitHub, GitLab, Linear, Slack, Sentry and Anthropic tokens by their prefixes. It cannot mask what it does not recognize, such as a configured secret shorter than 8 characters, one the agent reformatted, or an unlabelled token with no known prefix, and the unredacted report stays in Claudear's database and execution logs, so don't rely on redaction to keep a secret out of `#releases`.

**Credentials.**

- The provider `env` (`[agent.providers.claude.env]`) is passed verbatim, with no `${VAR}` expansion, to every agent run of every source (fix runs, Q&A and replies as well as live QA), and so is the daemon environment the agent inherits. A QA key set there reaches all of them. The one exception: a live-QA run gets no variable named `CLAUDEAR_*` from either, such as `CLAUDEAR_MASTER_KEY` or Claudear's GitHub and Discord tokens, so give QA credentials other names.
- An MCP server's `env` values can reference `${VAR}`, which Claude Code expands from the agent's environment, so the secret itself stays out of the config file. The only `CLAUDEAR_*` variable in a live-QA run's environment is `CLAUDEAR_RUN`, the per-run marker Claudear uses to find and kill the processes the run leaves behind, so any other `${CLAUDEAR_*}` reference never resolves there.
- `@playwright/mcp` does not read logins from environment variables. Give it a saved session with `--storage-state <file>` (cookies and local storage, e.g. saved with Playwright's `storageState()`), or `--secrets <dotenv file>`, whose keys the agent types in place of the values; Playwright MCP also masks those values in its tool output, as a convenience rather than a security boundary.

**Browser.** The commented Playwright entry in [`claudear.example.toml`](claudear.example.toml) attaches a headless browser to `deploy_qa` runs only (`sources = ["deploy_qa"]`). It needs Node.js and `npx` on the daemon host, plus the Chromium build that exact `@playwright/mcp` version expects and its OS libraries: `npx -y @playwright/mcp@0.0.82 install-browser --with-deps chromium` (`--with-deps` installs system packages, so it needs root or sudo). Pin the version as the example does rather than using `@latest`, which can move to a Playwright release whose browser build is not installed, and keep `--browser chromium` so the server uses that build instead of looking for Google Chrome. The Docker image ships neither Node.js nor a browser.

Requires a GitHub token (`scm.github.token` or `CLAUDEAR_GITHUB_TOKEN`) for release polling: config validation rejects an enabled `[deploy_qa]` with tracks but no token. A live host also needs a Discord bot token with message + create-thread permissions.

### Release Tracking

Dependency-aware release detection that tracks when fixes land in production.

```bash
# Show the release dependency graph
claudear diag release-graph

# Check if a PR's fix is in a target release
claudear diag release-check owner/repo 42 --target owner/target-repo

# Show the dependency path from source to target
claudear diag release-path owner/source-repo owner/target-repo
```

### Diagnostics

```bash
# Database stats and recent operations
claudear diag db
```

### Code Chat

Ask questions about your indexed codebase using a local LLM.

```bash
# Interactive REPL
claudear chat

# Single question
claudear chat "Where is the auth middleware defined?"

# Scope to a specific repo
claudear chat --repo my-org/my-repo "How does retry logic work?"

# Use a specific model file
claudear chat --model ~/.cache/claudear/models/custom.gguf

# Download the default model if missing
claudear chat --download-model
```

Requires `[llm]` and `[chat]` to be enabled in your config. The chat feature uses RAG (retrieval-augmented generation) over your indexed repositories.

### Dry Run

```bash
# Preview what would be processed without actually running Claude
claudear dry-run
```

### User Registry

The user registry maps each person to their handles across services, so assignee matching, mentions and notifications reach the right person. It merges two sources: identities saved on dashboard users ("console identities") and `[users.<key>]` entries in `claudear.toml`.

```toml
[users.jake]
linear_names = "Jake Barnby"
github_usernames = "jakebarnby"
sentry_usernames = "jake"
jira_usernames = "jake.barnby"
gitlab_usernames = "jakebarnby"
discord_id = "123456789012345678"
slack_id = "U0123456789"
email = "jake@example.com"
push_user_key = "pushover_user_key"
sms_number = "+1234567890"
whatsapp_number = "+1234567890"
telegram_chat_id = "123456789"
```

When an issue is assigned to a user, Claudear routes notifications to their configured channels.

#### Console identities

Every logged-in user sees **Users** in the dashboard sidebar:

- **Admins** see every user with an Identities column and edit anyone's identities, person key included, from the user's edit panel.
- **Everyone else** sees **My identities** there, and the same form under **Settings > My Identities**. They edit only their own handles and cannot change their person key.

The form groups the person key, source handles (Linear, GitHub, Sentry, Jira, GitLab), chat (Discord ID, Slack ID) and notification targets (email, Pushover user key, SMS, WhatsApp, Telegram chat ID). Admins edit the slug the person key comes from; everyone else sees their effective person key read-only. A badge shows where the values come from: **Console**, **From config (key)** or **Not set**. **Revert to config** drops the console identities so the person falls back to their `[users]` entry. When a non-admin reverts, a slug an admin set to anything other than the config key their login email matches is kept, and only the identities are cleared. Otherwise the row is removed and their person key falls back to config: the `[users]` key their login email matches, or none (**Not set**) when no unclaimed config entry matches, so a `console-<user id>` key does not survive.

#### Precedence

1. A console user's identities replace the `[users.<key>]` entry with the same person key, in full: a field set only in config does not survive the override.
2. `[users]` entries without a console override stay active, as seed and fallback.
3. `github-discord-map.json` is consulted only when the person who owns that GitHub login in the registry has no `discord_id`, or no person owns the login.

A console user's person key is, in order:

1. the slug an admin set on them;
2. otherwise the `[users.<key>]` entry whose `email` matches their **login** email, case-insensitively, unless an admin-set slug already claims that key;
3. otherwise `console-<user id>`.

Login emails are compared case-insensitively everywhere (login, user creation, the bulk load and config matching) and stored trimmed and lowercased, so `Amy@Example.com` and `amy@example.com` are one user. The identity `email` field never links a console user to a config entry, because users edit it themselves; only the login email (which only admins change) or an admin-set slug does. `console-<user id>` always stays an alias, so issues resolved to it earlier keep resolving after a slug is added. The first time someone saves identities for a person who comes from config, the config values are the starting point and the config key becomes their slug, so saving one field never drops the others.

When a handle is claimed twice, console users win over config entries, then lower user ids and keys win. GitHub and GitLab logins match case-insensitively; every other handle matches exactly.

Edits apply without a restart: instantly in the process that served the write, and within one housekeeping cycle in any other process sharing the database, such as a standalone `claudear dashboard`.

Global notifier references, such as `[notifiers.discord] user_id = "jake"`, `[notifiers.slack] user_id`, `[notifiers.email] to_addresses`, `[notifiers.push] user_key` and `[notifiers.sms] to_numbers`, are resolved to a person's handle each time a notification is sent rather than once at startup, so console edits apply to them too. A value that names no person is used as-is. The weekly repetitive digest's on-call mention resolves through the registry the same way.

#### Who can do what

| Action | Admin | Any other logged-in user (viewers included) |
|--------|-------|---------------------------------------------|
| Read the directory (`GET /api/identities`) | Everyone, in full | Everyone; other users redacted |
| Read one user's identities | Anyone, in full | Self in full; others redacted |
| Edit identities or revert to config | Anyone | Self only (403 otherwise); reverting keeps an admin-set slug |
| Set a person key (`slug`) | Yes | No (403) |
| Create users, issue or revoke invites | Yes | No (403) |
| Bulk load (`PUT /api/directory`) | Yes, by session or Bearer API token | No (403) |

A handle (any GitHub, GitLab, Linear, Sentry or Jira entry, `discord_id`, `slack_id`) or slug that already belongs to another person, console or config, is rejected with 409 for every role, so nobody can redirect someone else's mentions. The 409 never names the other person. Notification targets (`email`, `sms_number`, `whatsapp_number`, `push_user_key`, `telegram_chat_id`) may be shared between people, for example a team inbox, and are never conflicts.

**Sensitive fields.** `email`, `push_user_key`, `sms_number`, `whatsapp_number` and `telegram_chat_id` are sensitive. When a non-admin reads another user, those fields and the user's login `email` are `null` and the response has `redacted: true`. Claudear never logs identity values or request bodies.

**Formats.** Writes are trimmed and de-duplicated; a leading `@` is stripped from GitHub and GitLab logins. Then:

| Field | Accepted |
|-------|----------|
| `discord_id` | 17-20 digits (a Discord user ID) |
| `github_usernames` | 1-39 letters, digits or `-`; no leading, trailing or double `-` |
| `gitlab_usernames` | 1-255 characters; a letter or digit, then letters, digits, `_`, `.` or `-` |
| `slack_id` | `U` or `W`, then 8-20 uppercase letters or digits (`U0123456789`) |
| `email` | A single address, at most 254 characters |
| `sms_number`, `whatsapp_number` | E.164: `+`, then 7-15 digits, the first not `0` (`+14155550123`) |
| `telegram_chat_id` | A numeric chat ID, optionally negative |
| `push_user_key` | A 30-character Pushover user key |
| `linear_names`, `sentry_usernames`, `jira_usernames` | Non-empty, at most 255 characters, no control characters |
| `slug` | A lowercase letter or digit, then up to 62 of `a-z`, `0-9`, `.`, `_`, `-`; must not start with `console-` |

These rules apply to dashboard and API writes only. `[users]` entries in TOML are not validated, so existing configs keep loading.

#### Invited users

An admin can add a person who has no password yet: leave the password blank under **Users > Add User**, call `POST /api/users` without `password`, or include an unknown email in `PUT /api/directory`. The user is created with `status: "invited"`:

- Their identities are live in the registry at once, so assignee matching, mentions and the deploy-QA `@releaser` reach them before they ever log in.
- They cannot log in, and their sessions and API tokens are ignored, until they accept. A login attempt gets the same 401 as a wrong password.

The invite link is `/invite#cldi_...`. It is single-use, expires after 7 days, and is shown only when issued: **Copy invite link** on the user's row issues a new one, which invalidates the previous link, so the dashboard asks first while that link is still open; **Revoke invite** cancels it. The secret sits in the URL fragment, which browsers never send to the server, so it stays out of access logs, Sentry and `Referer` headers; the invite page also strips it from the address bar. The person opens the link, sets a password (8-72 bytes of UTF-8, so fewer characters for non-ASCII passwords), then signs in normally; accepting does not log them in and removes the secret from the page's history entry.

Setting a password any other way also activates an invited user and revokes their open invite: an admin setting `password` with `PUT /api/users/{id}`, or `claudear users seed` for that email.

Set `[dashboard] public_url` (or `CLAUDEAR_DASHBOARD_PUBLIC_URL`) to the address people use to reach the dashboard and the API returns absolute links in `invite_url`. It must be the dashboard's origin (scheme, host and optional port) with no sub-path: the dashboard is served from `/`, so a link under a sub-path such as `https://example.com/claudear/invite#...` does not open the invite page. Without it `invite_url` is `null`; prefix `invite_link` with your dashboard's address (the dashboard does this for you with the address in your browser).

```toml
[dashboard]
public_url = "https://claudear.example.com"
```

#### Identity API

Unless noted, these endpoints need a dashboard session cookie and return 401 without one. Every `POST`, `PUT` and `DELETE` also needs an `x-csrf-token` header matching the `claudear_csrf` cookie, which the dashboard sets on every response. The one exception is `PUT /api/directory` sent with an `Authorization: Bearer` API token: it is the only dashboard endpoint that accepts Bearer, and with Bearer it needs no CSRF header. Writes are rate-limited per user and return 429 over the limit.

Every non-2xx response from the users, identities, invite and directory endpoints, including 401 and 403 rejections, is JSON with a human-readable `error`, plus `violations` when specific fields are at fault (most 400 and 409 responses):

```json
{ "error": "A user with this email already exists" }
{ "error": "Invalid identities", "violations": [{ "field": "discord_id", "message": "Discord ID must be a 17-20 digit Discord user ID" }] }
```

Typical messages are `"Not authenticated"`, `"Invalid or expired token"` (a bad Bearer token), `"Admin access required"`, `"Invalid role"`, `"Password must be 8-72 bytes"`, `"A user with this email already exists"`, `"User not found"`, `"You can only change your own identities"` and `"Only an admin can change the person key"`. `POST /api/auth/login` keeps its own 401 body.

| Endpoint | Who | Result |
|----------|-----|--------|
| `GET /api/identities` | Any logged-in user | `UserIdentities[]` for every user; non-admins get others redacted |
| `GET /api/users/{id}/identities` | Any logged-in user | `UserIdentities`; redacted for a non-admin reading someone else. 404 if the user does not exist |
| `PUT /api/users/{id}/identities` | Admin, or self | Sparse update; returns `UserIdentities`. 400 invalid values, 403 not admin and not self or a non-admin sending `slug`, 404 no such user, 409 a handle or slug belongs to another person |
| `DELETE /api/users/{id}/identities` | Admin, or self | Drops the console identities (revert to config); returns the now-effective `UserIdentities`. An admin removes the row entirely. A non-admin whose row has an admin-set `slug` other than the config key their login email matches keeps that slug and loses only the identities; otherwise their row is removed too, which keeps the same person key. 403, 404 as above |
| `POST /api/users` | Admin | Creates a user; returns 201 `CreatedUserResponse`. `password` (8-72 bytes) is optional: without it the user is invited and `invite` is set. Optional `identities` takes an update body. 409 if the email exists, compared case-insensitively |
| `POST /api/users/{id}/invite` | Admin | Issues or re-issues an invite; the previous link stops working. Returns `UserInvite`. 404 no such user, 409 `{"error": "User is already active"}` |
| `DELETE /api/users/{id}/invite` | Admin | Revokes the open invite; returns `{"message": "Invite revoked"}`. 404 if there is none |
| `POST /api/auth/invite/inspect` | Anyone with the link | Body `{"token": "cldi_..."}`; returns `{"name", "email", "expires_at"}` |
| `POST /api/auth/invite/accept` | Anyone with the link | Body `{"token": "cldi_...", "password": "..."}`; returns `{"message": "Invite accepted"}`. 400 if the password is not 8-72 bytes |
| `PUT /api/directory` | Admin, by session or Bearer | Bulk create-or-update by email, all or nothing |

The two invite endpoints need no session but still need the CSRF header, which the invite page sends. Any invalid, expired, used or revoked token gets the same 404 `{"error": "This invite link is invalid or has expired"}`. They are rate-limited per IP in a bucket of their own, separate from login, and return 429 over the limit.

`GET /api/users` and `GET /api/users/{id}` stay admin-only; their user objects now include `status` (`"active"` or `"invited"`) and `invite_expires_at` (the open invite's expiry, or `null`).

`UserIdentities`:

```json
{
  "user_id": 3,
  "name": "Jake",
  "login_email": "jake@example.com",
  "status": "active",
  "person_key": "jake",
  "slug": "jake",
  "origin": "console",
  "redacted": false,
  "identities": {
    "linear_names": [],
    "github_usernames": ["jakebarnby"],
    "sentry_usernames": [],
    "jira_usernames": [],
    "gitlab_usernames": [],
    "discord_id": "452316113016193024",
    "slack_id": null,
    "email": null,
    "push_user_key": null,
    "sms_number": null,
    "whatsapp_number": null,
    "telegram_chat_id": null
  },
  "updated_at": "2026-11-04 10:00:00"
}
```

- `login_email` is the user's console login email, `null` when `redacted`. It is not the identity `email` under `identities`, which is a notification target.
- `person_key` is the effective registry key: the admin-set slug, the matching `[users.<key>]` key, or `console-<user id>`. It is `null` only when `origin` is `"unset"`.
- `slug` is the slug an admin set explicitly, or `null`. It is what an update's `slug` field writes, and it is always `null` when `origin` is `"config"`.
- `origin` is `"console"` when the user has saved identities, `"config"` when they have none but their login email matches a `[users.<key>]` entry (identities come from config and `person_key` is that key), and `"unset"` otherwise (identities are empty).
- `updated_at` is `null` unless `origin` is `"console"`.

An update body names only the fields to change; every field is optional and unknown fields are rejected:

```json
{ "slug": "jake", "github_usernames": ["jakebarnby"], "discord_id": "452316113016193024", "sms_number": "" }
```

An absent field is left as it is; `""` clears a single value and `[]` clears a list.

`UserInvite`, returned by invite issue, user creation and the bulk load:

```json
{
  "invite_link": "/invite#cldi_...",
  "invite_url": "https://claudear.example.com/invite#cldi_...",
  "expires_at": "2026-11-11 10:00:00"
}
```

`CreatedUserResponse` is the user object (with `status` and `invite_expires_at`) plus `"invite": UserInvite | null`.

Validation errors (400):

```json
{
  "error": "Invalid identities",
  "violations": [{ "field": "discord_id", "message": "Discord ID must be a 17-20 digit Discord user ID" }]
}
```

#### Bulk population

`PUT /api/directory` creates or updates many people at once, matched by login email. An unknown email creates an invited user with an invite and needs `name` and `role` (`admin` or `viewer`). A known email gets a sparse update of its identities and, optionally, its `name` and `role`. Every entry is validated before anything is written, including duplicate emails in the batch and handle conflicts within the batch and against the registry. One bad entry writes nothing.

To load a directory with curl:

1. As an admin, open **Users**, edit your own user and create an API token under **API Tokens**. Copy the `cldr_...` secret; it is shown once.
2. Write `people.json`:

   ```json
   {
     "users": [
       {
         "email": "amy@example.com",
         "name": "Amy",
         "role": "viewer",
         "identities": {
           "linear_names": ["Amy Chen"],
           "github_usernames": ["amychen"],
           "discord_id": "452316113016193024",
           "slack_id": "U04ABCD1234"
         }
       },
       {
         "email": "ben@example.com",
         "name": "Ben",
         "role": "admin",
         "identities": {
           "github_usernames": ["ben-dev"],
           "gitlab_usernames": ["ben"],
           "discord_id": "381947520183746561"
         }
       },
       {
         "email": "jake@example.com",
         "identities": { "linear_names": ["Jake Barnby"], "sentry_usernames": ["jake"] }
       }
     ]
   }
   ```

3. Send it:

   ```bash
   BASE=https://claudear.example.com
   CLAUDEAR_TOKEN=cldr_...
   curl -sS -X PUT "$BASE/api/directory" \
     -H "Authorization: Bearer $CLAUDEAR_TOKEN" -H 'content-type: application/json' \
     -d @people.json | jq '.users[] | {email, created, link: (.invite.invite_url // .invite.invite_link)}'
   ```

4. Send each new person their link, then revoke the token under **API Tokens**.

A Bearer request ignores cookies. A bad, expired or revoked token gets 401, and so does a token whose owner is still invited, because invited users' tokens are ignored until they accept; an active viewer's token gets 403. Any admin API token, including one minted for MCP, can call this endpoint, so use a dedicated token for the load and revoke it afterwards.

The response lists each entry in request order:

```jsonc
{
  "users": [
    { "user_id": 7, "email": "amy@example.com", "created": true, "status": "invited",
      "identities": /* UserIdentities */, "invite": /* UserInvite */ },
    { "user_id": 1, "email": "jake@example.com", "created": false, "status": "active",
      "identities": /* UserIdentities */, "invite": null }
  ]
}
```

Failures use the same error shape as the other identity endpoints, with the entry's `email` on each violation and `field` one of `email`, `name`, `role` or an identity field: 400 `{"error": "Invalid directory entries", "violations": [{"email": "amy@example.com", "field": "discord_id", "message": "..."}]}` for invalid entries, and 409 in the same shape for handle or slug conflicts. Each result row's `identities` is a `UserIdentities`.

#### Known limitations

Handle and slug conflicts are checked against the registry as it was before the write, so two changes that are only consistent once both apply are rejected with 409. Make them in two steps:

- **Relinking a config-matched user to a different slug.** While a user's identities still come from config (`origin: "config"`), the config handles copied into the write still belong to that `[users.<key>]` entry, so setting a new `slug` in the same request conflicts with it. Save their identities first, which makes them a console user with that key as their slug, then change the slug. The old `[users.<key>]` entry is no longer overridden after the relink and becomes active again; remove it from `claudear.toml` if it is no longer needed.
- **Moving a handle between two people in one `PUT /api/directory` batch.** Remove the handle from the first person in one request, then add it to the second in another.

---

## Fix Attempt Lifecycle

```
┌─────────┐     ┌─────────┐     ┌─────────┐     ┌─────────┐
│ Pending │────>│ Success │────>│ Merged  │────>│Resolved │
└─────────┘     └─────────┘     └─────────┘     └─────────┘
     │               │               │
     │               │               ▼
     │               │          ┌─────────┐
     │               └─────────>│ Closed  │──┐
     │                          └─────────┘  │
     │                                       │ retry
     ▼                                       │
┌─────────┐                                  │
│ Failed  │<─────────────────────────────────┘
└────┬────┘
     │ retry (max 2)
     ▼
┌───────────┐
│Cannot Fix │
└───────────┘
```

| Status | Description |
|--------|-------------|
| **Pending** | Fix attempt in progress |
| **Success** | PR created successfully |
| **Merged** | PR merged, issue auto-resolved on source |
| **Closed** | PR closed without merging (triggers retry) |
| **Failed** | Fix attempt failed (triggers retry) |
| **Cannot Fix** | Max retries exhausted |
| **Declined** | A human refused approval; not retried or asked about again unless reset |

---

## AI Feedback Loop

Claudear learns from every fix attempt to improve future performance through multiple feedback channels.

### Embedding-Based Learning
1. **Track Outcomes**: Every attempt result (merged, closed, failed) is recorded
2. **Generate Embeddings**: Issue content is vectorized locally using fastembed (ONNX Runtime)
3. **Find Similar Issues**: When a new issue arrives, similar past issues are retrieved
4. **Extract Patterns**: Keywords and strategies from successful fixes are extracted
5. **Enhance Prompts**: Future prompts are augmented with learnings from similar past issues

All embedding computation runs locally. No external API calls. Supported models: `nomic`, `minilm`, `bge`.

### Continuous Learning Pipeline
- **Execution Log Analysis**: Auto-extracts learnings from Claude's tool use logs
- **PR Diff Analysis**: Analyzes what changed in merged PRs to extract coding patterns
- **Q&A Knowledge Promotion**: Repeated Q&A answers are promoted to standing instructions
- **Per-Repo Knowledge**: Accumulates repository-specific knowledge from successful fixes
- **Review Feedback Classification**: Learns from code review comments on PRs
- **Strategy Fingerprinting**: Tracks how Claude approaches different issue types
- **Quality Scoring**: Scores fix quality based on merge velocity
- **Cluster Detection**: Detects correlated issues within configurable time windows
- **Cross-Repo Correlation**: Identifies failure patterns across repository boundaries
- **Auto AGENT.md**: Optionally generates `AGENT.md` from accumulated knowledge

---

## Custom Prompt Templates

### Per-Repository: AGENT.md

Create an `AGENT.md` file in any repository root. Claudear prepends this to all prompts for that repo.

```markdown
# Project Guidelines

## Code Style
- Use TypeScript strict mode
- Follow Airbnb style guide
- Keep functions under 50 lines

## Testing
- Write unit tests for all new functions
- Aim for 80% code coverage
- Use Jest for testing

## PR Guidelines
- Keep PRs focused on a single issue
- Include tests with all changes
```

### Global: claudear.toml

```toml
[agent]
# Default provider to use ("claude", "codex", etc.)
default_provider = "claude"

# Agent process execution timeout in seconds (default: 21600 = 6 hours)
timeout_secs = 21600

# Use the local LLM as the agent runner instead of an external provider (default: false)
# Requires [llm] to be enabled. Fully offline but much slower. Creates PRs via `gh` CLI.
use_llm = false

[agent.providers.claude]
# Model selection
model = "sonnet"   # sonnet, opus, haiku, or full model ID

# Cheaper model for repo classification (optional, falls back to model)
# classification_model = "haiku"

# Custom instructions appended to the system prompt
instructions = "Always write tests. Follow existing code style."

# Or load from a file
instructions_file = "./claude-instructions.md"

# Tool permissions granted without prompting
permissions = ["Bash(git *)", "Read", "Edit"]

# Skip all permission prompts (default: true)
skip_permissions = true

# CLI binary path (set when daemon can't find it via PATH)
# binary = "/home/user/.local/bin/claude"

# Extra env vars for the agent process (useful for systemd services)
# [agent.providers.claude.env]
# PATH = "/home/user/.local/bin:/home/user/.nvm/versions/node/v20/bin:/usr/local/bin:/usr/bin:/bin"
```

### A/B Experiments

Test different providers or configurations against each other:

```toml
[[agent.experiments]]
name = "claude-vs-codex"
enabled = true
strategy = "weighted_random"   # "weighted_random" or "fallback"

[[agent.experiments.providers]]
name = "claude"
weight = 0.8

[[agent.experiments.providers]]
name = "codex"
weight = 0.2
```

### Local LLM

Optional local model for offline repo classification and code chat:

```toml
[llm]
enabled = false
model_path = "~/.cache/claudear/models/qwen2.5-coder-3b-instruct-q4_k_m.gguf"
gpu_layers = 99          # 0 = CPU only, 99 = all layers on GPU
inference_timeout_secs = 120

# Use the configured agent (claude/codex) for repo classification instead of
# the local model. Much faster but costs API credits. (default: false)
use_agent = false
```

---

## Running as a Service

Service managers need `claudear start --foreground`: without it, `start` forks into the background and the manager loses track of the daemon. Give Claudear 50 seconds to stop before the manager sends SIGKILL: that covers the 30-second drain (see [Graceful Shutdown](#graceful-shutdown)), up to 5 seconds for interrupted agent CLIs, up to 5 seconds for blocking work to stop, and a margin. The defaults fall short of that in Docker (10 seconds) and launchd (20 seconds); systemd's 90 seconds would do, and the unit below sets 50 like the other examples.

### macOS (launchd)

Create `~/Library/LaunchAgents/com.claudear.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.claudear</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/claudear</string>
        <string>start</string>
        <string>--poll</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ExitTimeOut</key>
    <integer>50</integer>
    <key>StandardOutPath</key>
    <string>/tmp/claudear.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/claudear.log</string>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.claudear.plist
```

`ExitTimeOut` is how long launchd waits after SIGTERM before it sends SIGKILL. `KeepAlive` restarts Claudear whenever it exits, so stop it with `launchctl unload ~/Library/LaunchAgents/com.claudear.plist` rather than `claudear stop`.

### Linux (systemd)

Create `/etc/systemd/system/claudear.service`:

```ini
[Unit]
Description=Claudear
After=network.target

[Service]
Type=simple
User=YOUR_USER
ExecStart=/usr/local/bin/claudear start --poll --foreground
KillMode=mixed
TimeoutStopSec=50
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now claudear
```

`KillMode=mixed` sends SIGTERM to Claudear only, so the agent CLIs it started get the SIGINT Claudear passes on rather than systemd's SIGTERM, and their runs can record how they ended; systemd kills any that are left once Claudear exits. `TimeoutStopSec=50` is how long systemd waits for Claudear to exit before it sends SIGKILL.

---

## Docker

### Docker Compose (Recommended)

```bash
# Start all services
docker compose up -d

# View logs
docker compose logs -f

# Stop
docker compose down
```

### Standalone

```bash
# Build
docker build -t claudear .

# Run with config file
docker run -d \
  -p 3100:3100 \
  --stop-timeout 50 \
  -v $(pwd)/claudear.toml:/app/claudear.toml \
  -v $(pwd):/app/workspace \
  -v claudear-data:/app/data \
  claudear

# Or with environment variable overrides
docker run -d \
  -p 3100:3100 \
  --stop-timeout 50 \
  -v $(pwd)/claudear.toml:/app/claudear.toml \
  -v $(pwd):/app/workspace \
  -v claudear-data:/app/data \
  -e CLAUDEAR_LINEAR_API_KEY=your-key \
  -e CLAUDEAR_GITHUB_TOKEN=your-token \
  claudear
```

### Stopping

`docker stop` sends SIGTERM, which tini forwards to Claudear, so it interrupts its agent CLIs and shuts down gracefully (see [Graceful Shutdown](#graceful-shutdown)). Docker kills the container if it is still running when the stop timeout runs out, and the default of 10 seconds is shorter than a shutdown can take: `docker-compose.yml` sets `stop_grace_period: 50s`, and the standalone examples pass `--stop-timeout 50`. For a container started without either, use `docker stop -t 50`.

Stop the container with `docker stop` (or `docker compose stop`) rather than running `claudear stop` inside it: the container ends as soon as Claudear exits, and a restart policy such as the compose file's `unless-stopped` starts it again unless Docker itself stopped it.

### Docker Details

The Docker image:
- Multi-stage build (Bun for dashboard, Rust for binary, Debian slim runtime)
- Includes Claude Code (native installer), git, the GitHub CLI (`gh`), `curl`, `jq` and `sqlite3`; it has no Node.js, `npx` or browser, so the `[deploy_qa]` Playwright browser needs an image built on top of it
- Runs Claudear under [tini](https://github.com/krallin/tini), which forwards signals and reaps orphaned processes
- Runs `claudear start --foreground` by default (webhooks and dashboard on port 3100); pass a command after the image name (or set `command:` in `docker-compose.yml`) to change it, e.g. `claudear start --foreground --poll`
- Embeds the dashboard UI in the binary
- Persists embedding model cache between restarts
- Supports both `ANTHROPIC_API_KEY` and OAuth login for Claude authentication
- Health check on `/api/health` every 30 seconds

> **Note**: SQLite WAL mode is incompatible with Docker Desktop's VirtioFS bind mounts. Use Docker named volumes (e.g., `claudear-data:/app/data`) instead of bind mounts for the database directory.

---

## CI/CD

The project includes GitHub Actions workflows:

### CI (`ci.yml`)
Runs on every push/PR to `main`/`develop`:
- Tests on Ubuntu
- Linting (rustfmt, clippy)
- Multi-platform builds (Linux, macOS Intel/ARM)
- Code coverage via tarpaulin
- Dashboard tests

### Release (`release.yml`)
Runs on version tags (`v*`):
- Creates GitHub release with changelog
- Builds binaries for Linux, macOS (Intel & ARM)
- Publishes Docker image to GHCR
- Publishes to Homebrew tap
- Builds and publishes Debian packages to APT repository

### Production E2E Smoke (`e2e-prod-smoke.yml`)
Manual/nightly live-flow verification:
- Creates a real Linear issue
- Runs `claudear trigger` against a real GitHub repo
- Verifies PR creation via GitHub API
- Cleans up (closes PR, resolves issue)
- Enable with repository variable `CLAUDEAR_PROD_E2E_ENABLED=true`

```bash
# Create a release
git tag v1.0.0
git push origin v1.0.0
```

---

## Development

### Prerequisites

- Rust 1.98+
- Bun (for dashboard)
- Docker (optional)

### Building

```bash
make build              # Debug build
make build-release      # Release build with embedded dashboard (optimized, LTO, stripped)
make install            # Install to /usr/local/bin
```

### Testing

```bash
make test               # Run Rust tests
make test-all           # Rust + dashboard tests
make test-prod-e2e      # Real production E2E smoke test (requires credentials)
make check              # Format + lint + test
```

**Production E2E test** requires these environment variables:
- `CLAUDEAR_E2E_LINEAR_API_KEY`
- `CLAUDEAR_E2E_LINEAR_TEAM_ID`
- `CLAUDEAR_E2E_GITHUB_REPO` (format: `owner/repo`)
- `CLAUDEAR_E2E_GITHUB_TOKEN`
- `CLAUDEAR_E2E_DISCORD_BOT_TOKEN` (for Discord scenarios)
- `CLAUDEAR_E2E_DISCORD_CHANNEL_ID` (for Discord scenarios)
- `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`

### Dashboard

```bash
make dashboard          # Install dependencies
make dashboard-dev      # Dev server on :5173
make dashboard-build    # Production build
make dashboard-test     # Run tests
```

### All Makefile Targets

| Target | Description |
|--------|-------------|
| `make build` | Debug build |
| `make build-release` | Release build (optimized, LTO, stripped) |
| `make install` | Install to /usr/local/bin |
| `make test` | Run Rust tests |
| `make test-all` | Run Rust + dashboard tests |
| `make test-prod-e2e` | Run production E2E smoke test |
| `make test-prod-e2e-docker` | Run E2E smoke test via Docker |
| `make lint` | Run clippy linter |
| `make fmt` | Format code |
| `make check` | Format + lint + test |
| `make dev` | Hot reload development |
| `make watch` | Watch mode for tests |
| `make doc` | Generate and open docs |
| `make audit` | Security audit on dependencies |
| `make dashboard-dev` | Dashboard dev server |
| `make dashboard-build` | Build dashboard for production |
| `make docker` | Build and start Docker services |
| `make docker-dev` | Development Docker environment |
| `make release-deb` | Build .deb package |
| `make db-reset` | Reset SQLite database |
| `make db-backup` | Backup SQLite database |

---

## License

MIT
