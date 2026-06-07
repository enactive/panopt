# PANopt Observe Layer - Design Document

Status: Draft (handoff)
Date: 2026-05-31
Location: `~/p/panopt/docs/observe-design.md`
Related: `../DESIGN.md` (PANopt core), `~/p/entirely/entire_checkpoint_review_workflow.md` (prior art), `~/p/yt-summarizer` (reference implementation of the generation engine)

## 1. Overview

The observe layer is PANopt's top altitude: a cross-project rollup of AI-agent and
human development activity, generated from the session transcripts that the
`entire` CLI already records per commit.

In one line: **the observe layer turns recorded agent sessions into tiered,
lens-driven summaries - per-checkpoint at the bottom, organization-wide at the
top - and serves them to humans (the cockpit) and agents (MCP).**

This is a direct extension of PANopt's thesis. The core `DESIGN.md` states that
"the single central observer *is* the product." The coordination daemon observes
todos, notes, and locks *within* one project. The observe layer observes
*completed work across many projects*. It is the panopticon widened from one cell
to the whole block.

The mechanism is one reusable object, the **observer** (Section 5), applied at
increasing scope. Generation of the lowest tier is cheap to start (a git hook on
one machine) and migrates to CI or a central service later with no rework, by
design (Section 6.3). The document ends with a four-phase build ladder
(Section 9) that is independently shippable at every rung.

## 2. Background and Motivation

### 2.1 What `entire` already gives us

`entire` (Entire CLI, version 0.6.1 at time of writing) records every
agent-assisted coding session and ties it to the Git commit it produced. It
stores a "checkpoint" per commit containing the full session transcript, the
human prompts, changed-file lists, token usage, and an agent-vs-human line
attribution. Commits carry an `Entire-Checkpoint: <12-hex-id>` trailer linking
the commit to its checkpoint.

This is an enormous, already-captured corpus of "what happened and why" that is
currently almost invisible. The motivation for the observe layer is to make it
*usable* - by people scanning what changed, and by agents needing historical
context - without anyone hand-writing summaries.

### 2.2 The problem with `entire`'s own summaries

`entire` can produce summaries, but the useful paths are gated behind its hosted
service (`entire login`):

- `entire checkpoint explain --generate` (AI summary) routes to the server and
  fails when logged out.
- `entire recap` and `entire activity` are server-backed.
- `entire dispatch` defaults to the server; its `--local` flag (generate via the
  locally installed agent CLI) only supports **GitHub** origin remotes and fails
  on self-hosted GitLab, which is where these repos live
  (`gitlab.enactive.net`).

We do not want to depend on the hosted service, and we want a **pluggable model**
(any model via OpenRouter), not whatever the service or the local agent CLI
picks. So the observe layer generates its own summaries.

### 2.3 Prior art in this codebase

Two existing artifacts shaped this design and should be read by anyone picking
it up:

- `~/p/entirely/entire_checkpoint_review_workflow.md` - an earlier proposal for
  keeping `Entire-Checkpoint:` trailers as durable pointers and reviewing context
  with helper scripts and CI, deliberately *not* committing summaries into the
  feature branch. The observe layer adopts its "pointer, not payload in the
  branch" instinct but resolves it differently (Section 6.4): summaries live on a
  dedicated `entire/summaries/v1` branch and/or a central DB, never in the
  working tree.
- `~/p/yt-summarizer` - a working Go pipeline that summarizes YouTube transcripts
  via OpenRouter. Its architecture is the template for the generation engine
  (Section 7). We lift its OpenRouter client almost verbatim and reuse its
  queue-plus-workers, validate-and-retry, and instruction-snapshot patterns.

## 3. Discoveries (verified)

These were established empirically against real repositories and are
load-bearing for the design. Anyone continuing this work should trust them but
can reproduce them with the commands in Appendix A.

### 3.1 The rich session data is on the Git remote, in plain files

`entire` pushes checkpoint data to a normal branch on the origin remote:

```
refs/heads/entire/checkpoints/v1
```

Fetching and listing that branch reveals a self-describing tree, sharded by
checkpoint id (`<first-2-hex>/<remaining-10-hex>/`), with one subdirectory per
session segment (`0/`, `1/`, ...):

```
04/38ec6cb0f9/metadata.json          <- checkpoint-level metadata
04/38ec6cb0f9/0/content_hash.txt     <- "sha256:..."  (idempotency key)
04/38ec6cb0f9/0/full.jsonl           <- full session transcript (JSONL)
04/38ec6cb0f9/0/metadata.json        <- per-segment metadata
04/38ec6cb0f9/0/prompt.txt           <- the human prompts
04/38ec6cb0f9/1/...                  <- second segment, same shape
```

This means **the entire input we need is readable with `git fetch` plus JSON/text
parsing - no `entire` binary, no login.** A service or CI job with a read token
can summarize any repo without the CLI installed at all.

Checkpoint-level `metadata.json` contains, among other fields:
`checkpoint_id`, `strategy`, `branch`, `checkpoints_count`, `files_touched[]`, a
`sessions[]` manifest (relative paths to each segment's metadata/transcript/
content_hash/prompt), `token_usage`, and `combined_attribution`
(`agent_lines`, `agent_removed`, `human_added`, `human_modified`, ...).

Per-segment `metadata.json` adds `session_id`, `created_at`, `agent`
(e.g. "Claude Code"), `model` (e.g. "claude-opus-4-7[1m]"), `turn_id`,
`session_metrics`, `initial_attribution`, and `prompt_attributions`.

`files_touched[]` is especially useful: it lets us validate that a generated
summary only references files the commit actually changed (Section 8) without a
`git show`.

### 3.2 Offline read paths that do work

With no login, against the local store, these are confirmed working and are an
alternative input source for local generation (Phase 1):

- `entire checkpoint list`
- `entire checkpoint explain <id|sha> --short | --full | --raw-transcript`
  (shows intent, prompts, files, full transcript, token counts)
- `entire search --json` is machine-readable and is already used by a
  Claude Code subagent in `~/p/yt-summarizer/.claude/agents/entire-search.md`.
  (Whether `search` needs auth was not verified; the subagent assumes it works.
  The remote-branch path in 3.1 is the dependency-free source of truth and should
  be preferred.)

### 3.3 Transcripts contain sensitive data

`prompt.txt` and `full.jsonl` are raw. Observed contents included internal
`solo://` resource references and could contain secrets, tokens, file paths, and
private context. Centralizing this data is a real trust-boundary decision
(Section 11), not an afterthought.

## 4. Goals and Non-Goals

### Goals

- Turn recorded `entire` sessions into useful summaries with no dependence on the
  `entire` hosted service.
- Use a pluggable model via OpenRouter, configurable per observer.
- Make summaries available to both humans (PANopt cockpit) and agents (MCP).
- Support custom, per-observer instructions ("lenses") so the same activity can
  be viewed many ways (review focus, standup, release notes, exec briefing).
- Climb from per-commit to organization-wide using one mechanism, shipping value
  at every rung.
- Keep generation location flexible (git hook, CI, or central service) without
  reworking consumers.

### Non-Goals

- Not a reimplementation of `entire`. We consume its recorded data; we do not
  replace its capture.
- Not a hosted product. This is personal/organizational tooling, consistent with
  PANopt's stance in the core `DESIGN.md`.
- Not committing summaries or transcripts into feature branches or working trees
  (carried over from the prior-art workflow doc).
- Not parsing `entire`'s internal storage formats beyond the documented branch
  layout in 3.1; if `entire` ships a stable machine-readable export later, prefer
  it.

## 5. Core concept: the observer

Every tier above the raw transcript is the same object at a different scope:

```
observer = scope    (which projects / repos / branches / devs / time window)
         + lens     (custom instruction text: the view you want)
         + cadence  (when to (re)generate: on-commit, on-push, hourly, on-demand)
         + sink     (where it renders: summaries branch, central DB, cockpit, API)
```

A per-checkpoint summary is the degenerate observer: scope is one checkpoint, the
lens is the default. An organization briefing is a wide-scope observer with an
exec lens. Climbing the ladder means widening scope and adding lenses - not new
machinery.

The **lens** is custom instruction text. The lens text in effect for a run is
snapshotted onto the produced summary (`lens_id` + `lens_text`), so a rollup is
reproducible and auditable even if the lens definition later changes. This
pattern is taken directly from `yt-summarizer`, which snapshots its global and
per-channel instruction text onto each video summary.

## 6. Key decisions

### 6.1 OpenRouter as the generation engine

Decision: generate via OpenRouter, not `entire --generate` (server-locked) and
not the local agent CLI (`dispatch --local` is GitHub-only and pins the model).

Rationale: model is pluggable per observer; no hosted-service dependency; works
on self-hosted GitLab. Reference client already exists in
`~/p/yt-summarizer/internal/llm/openrouter.go` (thin chat-completions client,
ephemeral prompt-cache markers, temperature 0.2, 5-minute timeout, empty-content
guard). Env contract mirrors yt-summarizer: `OPENROUTER_API_KEY` enables
generation; a model env var selects the default model.

### 6.2 Multi-tier summarization

Decision: summarize in tiers. Tier 1 is per-checkpoint, generated from the raw
transcript. Tier 2 and above are rollups generated *from lower-tier summaries*,
not from transcripts.

Rationale: the expensive transcript pass happens once. Rollups summarize already
condensed text, so cross-project and organization views are cheap and fast. This
mirrors what `entire recap`/`dispatch` do conceptually, but cross-project,
self-hosted, and model-pluggable.

### 6.3 Generation location is pluggable; aggregation is central

Decision: tier-1 generation may happen in a git hook, a CI step, or the central
service - interchangeably. Aggregation (tier 2+) is always central (the observe
layer).

Rationale and the invariant that makes it work: **tier-1 summaries are
generate-once, keyed by `content_hash.txt`** (Section 3.1). Any producer writing
the same artifact at the same key is equivalent, so the two deployment shapes the
project considered are complementary, not a fork:

- "Service pulls the checkpoints branch and summarizes into a DB" and
- "CI step summarizes on push and writes to the summaries branch"

both produce the identical artifact. The central observer ingests existing
summaries for free and generates only the missing ones. A per-repo CI job cannot
see across projects, so it can never be the aggregator; that is the observe
layer's job regardless of where generation ran.

### 6.4 Storage: a summaries branch plus a central DB

Decision: write tier-1 summaries to a dedicated branch `entire/summaries/v1`
mirroring the layout of `entire/checkpoints/v1`, and also into the central
observe DB. Never into the working tree.

Rationale: the branch is a decentralized read-cache - any clone or agent can
fetch summaries with plain Git and the same repo token, with no live dependency
on the service. The DB is the central, queryable source for rollups and the
cockpit. This honors the prior-art doc's "no summaries in the feature branch"
rule (a dedicated ref is not the working tree) while keeping summaries
"committed" and shareable, which was the user's stated preference over git notes.
Git notes were considered and set aside as less discoverable than a branch that
mirrors the layout developers already see for checkpoints.

### 6.5 The observe layer lives in PANopt

Decision: the aggregation, access, and display layers are PANopt features, not a
standalone product. Per-checkpoint and rollup summaries become first-class
PANopt resources served over the existing MCP transport and rendered in the
cockpit.

Rationale: PANopt is already the central observer with an MCP server, a notes
resource model, and a five-pane cockpit with a markdown content pane. The
observe layer is the cross-project generalization of exactly that. Building it
elsewhere would duplicate the daemon, the MCP surface, and the UI.

Consequence and the one genuinely new piece of architecture: PANopt's MCP and id
counter are **per-project today** (each connection scoped via `?ws=<abs-path>`).
The observe layer is the first **cross-workspace** view. Phase 4 (Section 9) is
where that cross-workspace tier gets designed; everything before it fits the
existing per-project model.

## 7. Architecture

### 7.1 Data flow

```
 entire/checkpoints/v1  (per repo, on the Git remote)
   full.jsonl + prompt.txt + metadata.json + content_hash.txt
        |
        |  TIER 1  generate-once, keyed by content_hash
        |  producer = git hook (P1) | CI step (P2) | central service (P2)
        v
 per-checkpoint summary  (schema in Section 8)
   written to entire/summaries/v1 branch  and/or  central observe DB
        |
        |  TIER 2+  observer = scope + lens, summarize summaries
        v
 rollups  (project multi-dev, then cross-project / organizational)
        |
        v
 ACCESS + DISPLAY
   MCP tools (observe.recap / observe.get / observe.search)
   cockpit "observe" pane + content pane
   optional web dashboard (later)
```

### 7.2 Components

- **Generator** (Go binary, name TBD, working title `entsum`): reads checkpoint
  segments, calls OpenRouter, validates, writes tier-1 artifacts. Runs identically
  in a hook, a CI job, or inside the service. Stateless apart from the
  content-hash idempotency check.
- **Enrollment registry**: the set of observed repos, each with a scoped repo
  token (read for ingest; optionally write to push the summaries branch). This is
  the "tokens per enrolled repo" idea. Tokens stored encrypted (the user already
  uses agenix in `~/p/nix-config`).
- **Ingest/aggregation service**: fetches each enrolled repo's
  `entire/checkpoints/v1` and `entire/summaries/v1`, ingests existing summaries,
  generates missing ones, runs tier-2+ observers, stores results.
- **Observe DB**: central store (SQLite to start, matching yt-summarizer; Postgres
  later if needed). Keyed by `content_hash` for tier 1; rollups keyed by
  (observer id, scope window).
- **PANopt surfaces**: MCP tools and a cockpit pane (Section 10).

### 7.3 Trigger model

The service is remote from the repos, so a remote signal is the right trigger:
GitLab push webhook (instant, needs an ingress) or polling `git ls-remote` for a
moved `entire/checkpoints/v1` ref (no ingress, slightly stale). Polling is the
honest MVP for self-hosted GitLab. The local git hook (Phase 1) is a separate,
machine-local trigger that produces the same artifact.

## 8. Artifact schema (Phase 0 deliverable)

Tier-1 per-checkpoint summary. Emitted as JSON, rendered to Markdown for display.

```jsonc
{
  "schema_version": 1,
  "content_hash":   "sha256:...",     // copied from content_hash.txt; the key
  "checkpoint_id":  "0438ec6cb0f9",
  "commit":         "9e5e9d1...",      // resolved via Entire-Checkpoint trailer
  "repo":           "gregzuro/yt-summarizer",
  "branch":         "multi-pod",
  "agent":          "Claude Code",
  "model_used":     "google/gemini-2.5-flash",  // the model that wrote THIS summary
  "session_model":  "claude-opus-4-7[1m]",      // the model that did the work
  "created_at":     "2026-05-24T17:34:57Z",

  "one_liner":      "...",            // for tables / rollup rows
  "tldr":           "...",            // 2-3 sentences
  "intent":         "...",            // the human's actual goal
  "changes":        ["...", "..."],   // claim-style bullets
  "key_decisions":  ["chose X over Y because ..."],
  "files":          ["home/git.nix"], // MUST be a subset of files_touched
  "commands":       ["nixos-rebuild switch ..."],
  "risks":          ["..."],
  "follow_ups":     ["..."],

  "attribution":    { "agent_lines": 312, "human_added": 0, ... },  // from metadata
  "lens_id":        "default",
  "lens_text":      "...",            // snapshot of the instruction used
  "generator_version": "entsum/0.1"
}
```

Validation: every entry in `files[]` must appear in the checkpoint's
`files_touched[]`. A hallucinated path triggers one corrective retry with the
valid set fed back, then the field is dropped rather than failing the run. This
is the analogue of yt-summarizer's anchor validation and is the main thing that
makes summaries trustworthy enough for agents to consume.

Rollup (tier 2+) schema is a superset: it replaces `content_hash`/`commit` with
an observer id and a scope descriptor (project(s), time window, dev/branch
filters), carries the same `lens_id`/`lens_text` snapshot, and references the
child summary ids it aggregated for drill-down.

## 9. Build ladder

Each phase is independently shippable and demoable. The content-hash invariant
(6.3) means later phases never force a rewrite of earlier ones.

| Phase | Altitude | Scope | Where it runs | New capability |
|---|---|---|---|---|
| 0 | contract | - | spec only | artifact schema + branch layout + hash key |
| 1 | per-checkpoint | 1 commit, 1 dev | local git hook | lowest-level generation |
| 2 | central collection | 1 repo, all checkpoints | service (+ CI option) | enrolled repos + tokens, one queryable store |
| 3 | project rollup | 1 project, all devs, window | PANopt (per-project) | tier-2 rollups; named lenses |
| 4 | organizational | many projects | PANopt cross-workspace | org observers; cross-project view |

### Phase 0 - The contract
Write `summary.schema.json` (Section 8) and a one-page `entire/summaries/v1`
branch-layout spec mirroring 3.1. Small, but it locks the artifact so generation
stays pluggable.
Exit: schema and layout doc committed.

### Phase 1 - Lowest-level generation (local, one dev)
A Go binary `entsum gen <commit|checkpoint>` (lift `internal/llm/openrouter.go`):
read the checkpoint segment(s), call OpenRouter with the default lens, validate
`files` against `files_touched`, retry once, write the tier-1 artifact. A
background `post-commit` hook fires it after Entire's own hook. Input source can
be the local store via `entire checkpoint explain --full` or the fetched branch;
prefer the branch path so the binary has no `entire` dependency.
Value: the committing dev instantly gets a clean summary; can pre-fill commit
body or PR text. Zero infrastructure.
Exit: every new commit gets a local summary; `entsum get HEAD` prints it;
re-running is a no-op (hash hit).

### Phase 2 - Central collection (first real observer)
Stand up the ingest service with the enrollment registry and per-repo read
tokens. For each repo, fetch both branches, ingest existing summaries, generate
the missing ones, store in the observe DB keyed by `content_hash`. Add the
CI-step generator as an alternative producer (push-time generation, write-back to
`entire/summaries/v1`) for repos that want it.
Value: one queryable place holding every checkpoint across all repos;
checkpoint-granularity recap across one or many. Home for "tokens per enrolled
repo."
Exit: service ingests 3+ repos; `observe recap --repo X` works; the CI path is
proven on one repo; hook, CI, and service all converge on identical artifacts.

### Phase 3 - Project rollups, multi-dev (tier 2, first lenses)
A project observer = scope(one project, time window, optional dev/branch filter)
+ a named lens. It summarizes the tier-1 summaries (cheap), snapshots the lens
text, writes a tier-2 rollup. Ship 2-3 starter lenses: team standup, "what should
I review", release-notes draft. Surface in PANopt as an observe pane plus MCP
tools, scoped to a single project - which fits the existing per-project MCP
scoping, so no architectural lift yet.
Value: a project, across all its devs, rendered through any lens, live in the
cockpit and to agents.
Exit: one project shows multi-dev rollups under 2+ named lenses, in cockpit and
MCP.

### Phase 4 - Organizational, cross-project
Design and build the cross-workspace tier above PANopt's per-project daemons (the
scoping consequence from 6.5). An org observer scopes across many projects -
"all my repos this morning", "everything security-relevant org-wide", "team
Foo's week" - each with its own lens, aggregating project rollups or drilling to
checkpoints. Consumers: a morning briefing across everything, an exec dashboard,
onboarding digests.
Value: the panopticon view - one observed surface across all projects, which the
core DESIGN.md calls the product.
Exit: a global observe view spanning 2+ projects under a custom org lens;
cross-workspace MCP and/or dashboard.

## 10. PANopt integration specifics

- **Display.** The cockpit already has a markdown content pane and a notes-style
  list model. An "observe" sidebar pane lists rollups (Today / This week / per
  project) and drills into per-checkpoint summaries. A web dashboard is a later
  sibling, not a Phase 1-3 requirement.
- **Agent access.** Summaries become first-class PANopt resources served over the
  existing MCP transport: `observe.recap(since, project?)`,
  `observe.get(checkpoint|commit)`, `observe.search(query)`. This supersedes the
  earlier idea of a separate MCP server or a dedicated summary-fetch subagent -
  it is just PANopt tools.
- **Scoping.** Per-project observers (Phase 3) fit the current `?ws=<abs-path>`
  model and shared per-project id counter. Cross-project observers (Phase 4)
  require a global tier above per-project scoping; this is the principal new
  design work and should be specced against `DESIGN.md` Sections 5 and 6 when
  Phase 4 begins.

## 11. Privacy and security

Transcripts are sensitive (3.3). The observe DB and the summaries branch can hold
secrets, internal references, and private context aggregated across many repos.
Consequences for whoever builds this:

- Access to the observe store and to the summaries branch must be at least as
  restricted as access to the source repos, and probably more, because it
  concentrates many repos' context in one place.
- Per-repo tokens should be least-privilege (read for ingest; a separate
  write-scoped token only where push-back is enabled) and individually revocable.
- Consider a lens-level redaction or "summary excludes verbatim secrets"
  instruction, and prefer storing generated summaries over raw transcripts in the
  central DB. Raw transcripts already live on the (access-controlled) checkpoints
  branch; the observe layer does not need to re-copy them.
- If any source repo is public, its checkpoint branch may be too - confirm before
  enrolling.

## 12. Reused assets

- `~/p/yt-summarizer/internal/llm/openrouter.go` - OpenRouter client, lift nearly
  verbatim.
- `~/p/yt-summarizer/internal/poll/summary.go` - queue-plus-workers, status state
  machine with startup reset of stuck rows, validate-and-retry, instruction
  snapshot, manual-inject escape hatch. The generation worker should follow this
  shape.
- `entire/checkpoints/v1` branch - the input corpus (3.1).
- PANopt daemon, MCP transport, notes resource model, and cockpit panes - the
  access and display substrate.
- `~/p/entirely/entire_checkpoint_review_workflow.md` - prior art for trailers and
  review tooling; reconciled in 6.4.

## 13. Open questions

1. **Cross-workspace tier (Phase 4).** Does the observe layer live inside the
   PANopt daemon as a new global tier, or as a federated sibling service the
   cockpit and MCP talk to? This is the biggest unresolved fork and gates Phase 4.
2. **Trigger.** Webhook vs poll for the ingest service (7.3). Poll is the MVP;
   webhook is the upgrade.
3. **Write-back.** Do all repos get a write token to push `entire/summaries/v1`,
   or is the central DB the only sink for some? Decides token scoping per repo.
4. **DB.** SQLite throughout, or Postgres once multi-writer/centralized? Start
   SQLite (matches yt-summarizer).
5. **Lens authoring UX.** Where are lenses defined and edited - a config file per
   observer, a PANopt resource, or both? Phase 3 needs an answer.
6. **`entire search` auth.** Confirm whether `entire search --json` works logged
   out; if so it is a convenient secondary index. The branch path (3.1) is the
   dependency-free source regardless.

## 14. Status and next step

This document is a handoff. Nothing is built yet. The recommended first move is
Phase 0 (the schema and branch-layout spec), immediately followed by Phase 1 (the
local generator plus hook), because Phase 1 delivers value to a single developer
with no service, no enrollment, and no tokens - and it exercises the OpenRouter
client and validation logic that every later phase depends on.

## Appendix A - Reproducing the discoveries

Run from inside an `entire`-enabled repo with a remote (verified against
`~/p/yt-summarizer` and `~/p/nix-config`, both on `gitlab.enactive.net`).

```sh
# Confirm the checkpoint branch exists on the remote
git ls-remote origin | grep entire
#   -> refs/heads/entire/checkpoints/v1

# Fetch it to a throwaway local ref and inspect the layout
git fetch origin 'refs/heads/entire/checkpoints/v1:refs/remotes/origin/entire-cp-inspect'
git ls-tree -r --name-only origin/entire-cp-inspect | head
#   -> <2hex>/<10hex>/{metadata.json, 0/{content_hash.txt,full.jsonl,metadata.json,prompt.txt}, ...}

# Read a checkpoint's metadata, prompts, transcript, and hash (no login)
R=origin/entire-cp-inspect
git show $R:04/38ec6cb0f9/metadata.json
git show $R:04/38ec6cb0f9/0/prompt.txt
git show $R:04/38ec6cb0f9/0/content_hash.txt
git show $R:04/38ec6cb0f9/0/full.jsonl | head

# Offline local read paths (alternative input for Phase 1)
entire checkpoint list
entire checkpoint explain <id|sha> --full --no-pager

# What requires the hosted service (do NOT depend on these)
entire checkpoint explain <id> --generate   # routes to server; fails logged out
entire recap                                 # server-backed
entire dispatch --local                      # GitHub-origin only; fails on GitLab

# Clean up the throwaway ref when done
git branch -rd origin/entire-cp-inspect
```
