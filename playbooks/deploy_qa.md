# Watch deploys playbook (deploy_qa)

Authoritative live-QA procedure for Appwrite Labs release tips. Encode this into Claudear `[deploy_qa]` agent instructions.

## Scope (repos / tags)

| Track | Repo | Tag filter |
|-------|------|------------|
| cloud | `appwrite-labs/cloud` | any release tag |
| edge-db | `appwrite-labs/edge` | tags with `-db` suffix |
| edge-network | `appwrite-labs/edge` | tags **without** `-db` |
| vibes (Console IV / frontend) | `appwrite/vibes` | any release tag |

Persist last-seen tip per track. Skip if unchanged. Skip-if-previous-still-running.

## Per new tip (before notifying humans)

1. Read release (tag, body, published time). List claimed merged PRs; verify each exists and merged; flag mismatches.
2. Classify each PR: **LIVE-TESTABLE** vs **INFRA/TEST-ONLY** (CI, path filters, unit-test deletions, composer/image bumps with no user-facing route, GitOps-only, docs-only).
3. Confirm GitOps pin in `application-configuration` for the right env. Pin is a precondition, not the QA result. Do not claim main is prod.
4. For every LIVE-TESTABLE PR: one-line test plan, then actually run it. Blocked ≠ pass.
5. API: HTTP client only (curl). Hosts: `cloud.appwrite.io`, `cloud.staging.appwrite.io`, regional fra/nyc/sfo/sgp/syd/tor, dedicated-DB hosts when that shipped. Health/version only as precondition, then routes the PR changed.
6. Console UI: rewrite preview only — `https://appwrite.io` / prod console rewrite (legacy `new.appwrite.io` → `appwrite.io`); staging `https://new.staging.appwrite.io`. Never classic `/console`. Debug menu: type `pink`. Enable Dedicated DBs support before DAT flows.
7. Dedicated-DB / DAT: throwaway project `qa-1044` / project id `6a8415b8002ea65eec9c` only. **Never** touch M01 or Production PostgreSQL.
8. Finish every LIVE-TESTABLE PR with pass / fail / blocked before reporting.

## Discord `#releases` (posted by Claudear)

Claudear posts the outcome under the automated release announcement in `#releases` from your report's verdict footer. **Do not post to Discord yourself** — no messages, replies, threads, or reactions.

- **All verified** (`ALL_VERIFIED`, nothing blocked): a reply on the release announcement, **no @**.
- **Unverified** (`UNVERIFIED`: nothing failed, but a LIVE-TESTABLE check was blocked or incomplete): a reply on the release announcement, **no @**.
- **FAIL** (`FAIL`: a LIVE-TESTABLE PR failed): a thread under the release announcement that **@s the releaser**, mapped from their GitHub login via `github-discord-map.json`.

Your report becomes the post, so keep it short: one bullet per PR, no curl dumps or URL walls. Detail stays in the attempt log.

## Non-goals

- Not regression / fix-inclusion tracking (`[regression]` / `ReleaseTracker`). That stays separate.
- Do not ingest `#releases` via Discord issue source (would spawn code-fix agents on bot posts).
- Do **not** open fix PRs, branches, or commits for a release announcement. Run the checks above and report; do not fix what they find.

## Required report format (machine-readable footer)

List each PR as `- #N title LIVE PASS|FAIL|BLOCKED` or `- #N title INFRA`.

End the attempt with exactly one of:

```
DEPLOY_QA_VERDICT: ALL_VERIFIED
```

```
DEPLOY_QA_VERDICT: UNVERIFIED
```

```
DEPLOY_QA_VERDICT: FAIL
```

- Any `LIVE FAIL` line means FAIL, whatever the footer says.
- Otherwise a `LIVE BLOCKED` line, an `UNVERIFIED` footer, or a missing or unrecognised footer means UNVERIFIED — blocked is never a pass.
- Only `ALL_VERIFIED` with no blocked PR counts as verified.
