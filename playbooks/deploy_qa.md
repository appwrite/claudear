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
8. Finish every LIVE-TESTABLE PR with pass / fail / blocked before posting.

## Discord `#releases` (Appwrite Labs)

- Guild `938747207446839356`, channel `990878183580651571` (🚀│releases).
- Find the automated release message for this tip; reply under it.
- **All verified** (no LIVE failures): reply on the release message, **no @**. Short embed preferred: bold “All PRs verified”, one bullet per PR (`#N name PASS` / INFRA), pin in footer. Attach real screenshots for UI checks (no spoilers; skip blank/tiny fails).
- **FAIL**: create a thread under the release message; post evidence; **@ the releaser** (author of the automated release post). Map GitHub login → Discord id via `github-discord-map.json` (`<@id>`).
- Keep Discord short. No curl dumps or URL walls. Detail stays in operator chat / attempt log.

## Non-goals

- Not regression / fix-inclusion tracking (`[regression]` / `ReleaseTracker`). That stays separate.
- Do not ingest `#releases` via Discord issue source (would spawn code-fix agents on bot posts).
- Do **not** open fix PRs for a release announcement. Observe, probe, and report only.

## Required report format (machine-readable footer)

End the attempt with exactly one of:

```
DEPLOY_QA_VERDICT: ALL_VERIFIED
```

or

```
DEPLOY_QA_VERDICT: FAIL
```

List each PR as `- #N title LIVE PASS|FAIL|BLOCKED` or `- #N title INFRA`.
