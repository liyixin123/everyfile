# Everyfile Agent Guidelines

## Development workflow

**Never commit code changes directly to `master`.**

Every change follows the issue-driven loop:

1. **Issue first.** Open a GitHub issue describing the question or task (body starts with a `## Question` for research/grilling tickets). Label it (`wayfinder:research` / `wayfinder:prototype` / `wayfinder:grilling` / `wayfinder:task`, or `ready-for-agent` when fully specified).
2. **One branch per issue.** Branch naming by kind: `codex/issue-<n>-<slug>` for implementation, `prototype/<slug>` and `research/<slug>` for exploratory work (these branches are kept but never merged into `master`).
3. **One PR per issue**, referencing the issue number.
4. **Squash merge.** The merge title is an imperative one-liner describing the delivered capability, with the PR number appended (e.g. `Recover safely from damaged file indexes (#32)`).
5. **Close the issue** after merge.

Each squashed commit should be a complete, releasable unit (code + tests + docs). Feature tickets deliver a matching `docs/architecture/*.md`; qualification tickets deliver `docs/qualification/*.md`.

Exceptions: trivial fixes (typos, doc tweaks) may go straight to `master`.

## Agent skills

### Issue tracker

Issues and specs are tracked in GitHub Issues via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Uses the default five-role triage vocabulary. See `docs/agents/triage-labels.md`.

### Domain docs

Uses a single-context domain documentation layout. See `docs/agents/domain.md`.
