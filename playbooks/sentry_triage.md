# Sentry triage

Decide whether this Sentry issue deserves a fix PR. A fix run is expensive, and a PR nobody merges costs reviewer time. When in doubt between `fix` and a skip verdict, pick `fix`. Only skip when the evidence is concrete.

## Verdicts

- `fix`: a defect in this codebase that a code change here removes. Examples: a missing null or bounds check, a wrong condition, an unhandled race, a 500 where the request was valid, a worker that crashes or stalls on a condition it should tolerate.
- `expected`: the code is doing its job. It rejects bad input or reports tenant state, and the exception is how that gets surfaced. Examples: "Project not found", "Collection not found", "Document already exists", permission and authorization errors for requests that really lack access, invalid cron expressions or URLs supplied by users, a certificate missing for a domain the user has not verified.
- `infra_transient`: a dependency or infrastructure failure that no code change here would remove. Examples: ClickHouse 502/503/timeouts, a dedicated MySQL or Redis host unreachable, DNS lookup failures, "Pool ... is empty" during an incident, orchestrator timeouts, "went away" connection errors.
- `noise`: logging of a handled condition that is already recovered from, health probes, or duplicates of another issue's symptom.
- `upstream_owned`: the defect is in another repository (a vendored `utopia-php/*` library, an SDK, a runtime). Put that repo in `owner_repo` as `org/repo`; the fix then runs there. Check where the library is developed: some `utopia-php/*` packages are read-only mirrors of `utopia-php/monorepo`.
- `needs_human`: fixing it needs a product or ops decision (limits, pricing, data cleanup, a migration on production data). The attempt stops so a person can decide; claudear will not pick it up again on its own.

## Rules

- An `expected` or `infra_transient` error can still be `fix` when the code mishandles it: it crashes a worker, aborts a batch, leaks a resource, retries forever, or returns 500 for what should be a 4xx. Say which in `evidence`.
- `evidence` must cite a file and line, or the concrete fact from the event (tag, message, frequency) that the verdict rests on. A skip verdict without evidence is treated as `fix`.
- Do not skip an issue just because you could not find the failing line. That is a `fix` with low confidence, or `upstream_owned` if the frame points into `vendor/`.
