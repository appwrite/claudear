# Watch deploys playbook (deploy_qa)

Authoritative live-QA procedure for Appwrite Labs release tips. Encode this into Claudear `[deploy_qa]` agent instructions.

## Scope (repos / tags)

| Track | Repo | Tag filter |
|-------|------|------------|
| cloud | `appwrite-labs/cloud` | any release tag |
| edge | `appwrite-labs/edge` | tags **without** `-db` |
| edge-db | `appwrite-labs/edge` | tags with `-db` suffix |
| console | `appwrite/appwrite` (monorepo) | `apps/console/<version>` tags (see **Console** below) |

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

## Console (`apps/console/<version>` tags)

The console is `apps/console` in the `appwrite/appwrite` monorepo: image `appwrite/new:<version>`, serving `https://appwrite.io` (marketing, docs and the signed-in console). Pushing an `apps/console/<version>` tag runs the `Console / Production` workflow, which builds the image, publishes assets to `https://cdn.appwrite.io`, and pins the version in GitOps. Console tags are bare: no GitHub release and no release notes. GitHub's release feed never announces them, so Claudear posts the `#releases` announcement (version, compare range, deploy run, releaser) when it picks the tag up. The repo's GitHub releases (`2.7.0`, `1.9.10`, ...) are the **server**, and `console/<version>` is the console SDK package; neither is a console deploy.

1. Changes: the release body gives the compare range since the previous console tag. List the commits in it that touch `apps/console/**` and their merged PRs (`gh api repos/appwrite/appwrite/compare/<previous>...<tag>`, then `gh api repos/appwrite/appwrite/commits/<sha>/pulls`). Commits outside `apps/console` ship in other deploys: list them as INFRA for this track.
2. Deployed version: the `Console / Production` run for the tag concluded `success` (`gh run list -R appwrite/appwrite --workflow console-production.yml --branch apps/console/<version>`; a run waiting for `console-production` approval or still running is BLOCKED), and `assets/production/console/default.yaml` in `appwrite-labs/application-configuration` pins `global.image.repository: appwrite/new` and `global.image.tag: <version>` (the version without `apps/console/`). The console does not render its own version, so this pin plus the successful run is the version check. A different pinned tag is FAIL; never infer it from `main`.
3. Reachability: `curl -fsS https://appwrite.io/health` returns `OK`; `https://appwrite.io/`, `/docs`, `/pricing` and `/sign-in` return 200; legacy `https://cloud.appwrite.io/console` redirects to `https://appwrite.io`.
4. Assets: every `https://cdn.appwrite.io/assets/...` script and stylesheet the `/` and `/sign-in` HTML references returns 200. A missing asset is FAIL: the page ships HTML for assets that never published.
5. Key pages (browser, signed in as the QA account): `/sign-in` renders; the organization view loads after sign-in; the QA project's `/projects/6a8415b8002ea65eec9c/overview`, `/databases`, `/auth` and `/settings` render without an error screen. Look, don't change anything, outside what a PR's own test plan needs in the QA project.
6. Console errors: on those pages, no uncaught exception and no failed (4xx/5xx) request to `cloud.appwrite.io/v1` or `cdn.appwrite.io` in the browser console or network log. Ignore third-party analytics the headless browser blocks. Quote the first error, trimmed, in the PR's result line.
7. Then test each LIVE-TESTABLE console PR as above. Staging (`https://new.staging.appwrite.io`) follows `main`, not the tag: use it only to reproduce, never as the result.

List steps 2 to 6 in the report as `- console deploy LIVE PASS|FAIL|BLOCKED` with the failing step named, before the PR lines.

## Discord `#releases` (posted by Claudear)

Claudear posts the outcome under the release announcement in `#releases` from your report's verdict footer. GitHub announces releases; Claudear announces bare tags, such as every console release, itself when it picks them up. If no announcement can be found, the outcome gets a post of its own naming the repo and tag. **Do not post to Discord yourself** — no messages, replies, threads, or reactions.

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
