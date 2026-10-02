# jev-loop

`jcode jev-loop` is a coding-agent loop with a Jev fork layer, ported from the
`jev-loop` Python prototype (slices 1 to 3) into jcode itself.

Opus plans and Sonnet helpers do each step. After each attempt the loop runs
the helper's check command itself, and rules in code settle the obvious
cases. When the check passed, Jev reviews the change against the step's
`done_when`: when Jev is confident it is met ("sharp"), code accepts the
step, and otherwise ("split") Opus decides. Jev also picks the files the
planner and each helper should start from.

## How a step is decided

1. The helper makes one attempt and reports, including the command that
   checks its work.
2. The loop runs that check command itself, from the repository root (see
   "Rules that live in code" for the protections). What the loop sees
   outranks what the helper claimed; a disagreement is logged and counted.
3. Rules settle every attempt whose check did not pass, with no model:
   - the tool-call or spending cap stopped the helper: escalate;
   - the helper reported a blocker: escalate;
   - the check failed, the loop could not run it, or the session failed
     (for example the provider was overloaded): retry, with the loop's own
     output as feedback;
   - the same error as the previous attempt (numbers such as timings and
     line numbers are ignored when comparing): escalate;
   - the attempt cap: escalate.
4. A check the loop saw pass goes to review. Jev answers one yes/no
   question: does the change (the step's diff, new files included, and the
   check's output) do what the step asks and meet every part of
   `done_when`? At or above the threshold, code accepts the step. Below it,
   or when Jev leans towards no, the Opus judge decides, with the same diff
   in hand.
5. Escalation asks Opus to rewrite the step once, then the run stops for
   you.

Before planning, and before each step, Jev also picks files: a keyword
search over the repository's files (`git ls-files`, with the task's paths and
identifiers searched for in file contents) finds up to 24 candidates, Jev
answers "will this task need this file?" for each, and the likely ones are
named in the planner's or helper's prompt as a place to start. A failed pick
leaves the prompt unchanged.

## What runs where

| Piece | Prototype | In jcode |
|---|---|---|
| Settings | `config.py` | `crates/jcode-app-core/src/jev_loop/config.rs` |
| The loop | `loop.py` | `jev_loop/engine.rs` |
| Claude calls | `claude_calls.py` (Claude Agent SDK) | `jev_loop/claude.rs` (jcode's own agent runtime) |
| Fork layer | `jev_layer.py` (`typesafe-sdk`) | `jev_loop/fork.rs` (jcode's native Jev client) |
| File picks | planned for slice 3 | `jev_loop/pick.rs` |
| Check runs, diffs, file search | none (the helper's word) | `jev_loop/workspace.rs` |
| Structured output | SDK `output_format` | `jev_loop/report.rs` (validated JSON reply) |
| Blocked commands | SDK `disallowed_tools` | `jev_loop/guard.rs` (enforced in code on every tool call) |
| CLI | `python loop.py` | `jcode jev-loop` (`src/cli/jev_loop.rs`) |

There is nothing extra to install: no Python, no venv, no SDKs. Claude calls
use whatever provider jcode is already logged in to, and Jev uses the Jev
client jcode already ships for memory, browser, and voice.

## Setup

1. Log in to Claude in jcode (`jcode login claude`), as for any session.
2. Give jcode your Jev key. Any one of these works; they are tried in order:
   - `TYPESAFE_API_KEY` in the environment, or in `~/.config/jcode/typesafe.env`
   - `OPENROUTER_API_KEY` (`openrouter.env`)
   - `AIMLAPI_API_KEY` (`aimlapi.env`)

   Pin one with `JCODE_LOOP_JEV_PROVIDER=typesafe|openrouter|aimlapi`. The loop
   never spends a Jcode subscription credential on forks.

## Run

Commit your work first. The loop refuses to start on uncommitted changes, so
everything it does can be reviewed with `git diff` and undone with git.
Working on a new branch is even better.

```bash
jcode jev-loop --repo ~/code/myproject --task "Add rate limiting to the login endpoint"
```

Two baselines for comparison. Both keep the rules and the loop's own check
runs, and neither calls Jev:

```bash
# Opus reviews every step whose check passed
jcode jev-loop --repo ~/code/myproject --task "..." --no-jev
# No review at all: a step is done when the loop sees its check pass
jcode jev-loop --repo ~/code/myproject --task "..." --rules-only
```

Run each on the same task (reset the repo between runs) and compare the
summaries: `--rules-only` shows what the rules alone achieve, `--no-jev`
what an Opus review adds, and the default what Jev adds for its cost. The
exit code is `0` when every step finished and `2`
when the run stopped for you to decide. A mistyped command line (for example
a missing `--task`, or `--no-jev` with `--rules-only`) also exits `2`, before
anything runs, so a script should
treat `2` as "stopped for you" only when the output contains the
`--- run summary ---` block. Any other error, such as a repository path that
does not exist or a malformed `JCODE_JEV_LOOP_*` value, exits `1`.

Name the check command exactly as it must run, including the interpreter,
for example `.venv/bin/python -m pytest -q tests/test_x.py` rather than
`python -m pytest`. Helper commands, and the loop's own run of each check,
use a login shell, which reapplies your shell profile's `PATH`, so a
virtualenv activated before starting the loop is not guaranteed to be first
on `PATH`. Helpers are told the loop reruns their check from the repository
root, so they report one complete command; a check the loop cannot run is
never accepted.

## Reading the fork log

Every fork and file pick is appended to `~/.jcode/jev-loop/forks.jsonl` (one
JSON line each; set `JCODE_JEV_LOOP_FORK_LOG` to change it). It lives
outside the target repo so the loop never dirties the tree it is editing.
For forks, the fields that matter:

- `name`: `step_rule` (a rule settled it) or `step_review` (a passing check
  was reviewed)
- `check`, `helper_said_passed`, and `loop_check`: what the loop's own run
  of the check showed (`passed`, `failed`, or `unverified` when it could not
  run it), what the helper claimed, and the run's exit code, time, and
  output end
- `answer` and `confidence`: what Jev said (`meets` or `falls_short`) and
  how sure it was
- `route`: `rule` (no model), `sharp` (Jev accepted the step), or `split`
  (Opus decided)
- `final_decision` and `decided_by`: what actually happened, and who decided
  (`jev`, `opus`, or `rule`)
- `reason`: why a rule or Opus decided what it did
- `note`: why Jev did not settle a review (unsure, doubtful, or unreachable)

File picks have `name` `file_pick`, the `step` (0 is the planner), the
number of `candidates`, the `files` suggested, and Jev's probability for
every candidate in `scores`.

Tuning the threshold: look at split reviews where Opus accepted a step Jev
leaned towards accepting. If Opus keeps agreeing at, say, 0.7, lower the
threshold toward 0.7 and save those Opus calls. If Jev ever accepted a step
that turned out wrong, raise it.

## Settings

Defaults match the prototype's `config.py`. Override without rebuilding:

| Variable | Default |
|---|---|
| `JCODE_JEV_LOOP_PLANNER_MODEL` | `claude-opus-5-5` |
| `JCODE_JEV_LOOP_HELPER_MODEL` | `claude-sonnet-5-5` |
| `JCODE_JEV_LOOP_PLANNER_EFFORT` | `high` |
| `JCODE_JEV_LOOP_HELPER_EFFORT` | `medium` |
| `JCODE_JEV_LOOP_THRESHOLD` | `0.80` |
| `JCODE_JEV_LOOP_MAX_ATTEMPTS` | `3` |
| `JCODE_JEV_LOOP_MAX_REVISIONS` | `1` |
| `JCODE_JEV_LOOP_HELPER_MAX_TOOL_CALLS` | `40` |
| `JCODE_JEV_LOOP_PLAN_MAX_TOOL_CALLS` | `40` |
| `JCODE_JEV_LOOP_JUDGE_MAX_TOOL_CALLS` | `8` |
| `JCODE_JEV_LOOP_REVISE_MAX_TOOL_CALLS` | `15` |
| `JCODE_JEV_LOOP_REVIEW_MAX_TOOL_CALLS` | `25` |
| `JCODE_JEV_LOOP_STEP_BUDGET_USD` | `2.00` |
| `JCODE_JEV_LOOP_CHECK_TIMEOUT_SECS` | `600` |
| `JCODE_JEV_LOOP_FORK_LOG` | `~/.jcode/jev-loop/forks.jsonl` |

Invalid values are rejected at startup instead of being ignored.

## Rules that live in code (not in prompts)

- A step is never accepted unless the loop itself saw its check pass. The
  prototype trusted the helper's `check_passed`; the port runs the check
  command again and decides on that. Only an attempt whose check passed
  ever reaches Jev or the Opus judge. The command is still the helper's own,
  so a helper can weaken it (for example by setting, inside the command, an
  environment variable the tests depend on). The loop runs exactly what was
  reported, and the Jev or Opus review sees that command; `--rules-only`
  has no review, so it accepts whatever command passes.
- The loop's run of a check gets the same protections as a helper's own
  commands: the blocked-command list below, no cloud or GitHub logins (see
  the safety note), jcode's destructive-command gate (a check it would hold
  is not run, and never with a justification), a time limit
  (`JCODE_JEV_LOOP_CHECK_TIMEOUT_SECS`, after which the check and every
  process it started are killed), and the repository root as its working
  directory.
- Failed checks, blockers, stopped attempts, and repeated errors are settled
  by the rules in "How a step is decided", not by a model.
- After `MAX_ATTEMPTS_PER_STEP` tries, a step is escalated no matter what,
  including when Opus says retry (a rule override).
- Opus may rewrite a stuck step once, then the run stops for you to decide.
- Blocked commands are refused for every session. The prototype's list
  (`git push`, `git reset --hard`, `git clean`, `rm -rf`, `sudo`) is extended
  with commands that change state outside the working tree, where `git diff`
  cannot show it and git cannot undo it:
  - whole programs: `gh`, `aws`, `gcloud`, `az`, `kubectl`, `ssh`, `scp`,
    `sftp`, `vercel`, `heroku`, `netlify`, `fly`, `wrangler`, and a few
    AWS deploy CLIs;
  - remote-changing subcommands of tools the loop still needs for checks:
    `terraform`/`tofu` `apply`, `destroy`, `import`, `refresh`, `state`, ...;
    `pulumi up`; `cdk`/`sam`/`serverless` `deploy`; `helm` `install`,
    `upgrade`, `rollback`, ...; `firebase deploy`; `git lfs push`;
  - image pushes (`docker push`, `docker build --push`, `podman push`, ...)
    and package registry changes (`npm`/`pnpm`/`yarn`/`cargo`/`uv`/`poetry`
    `publish`, `twine upload`, `npm unpublish`, `cargo yank`, `gem push`, ...).

  `terraform plan`/`validate`/`init`, `helm lint`/`template`, `cdk synth`,
  `sam build`, and local `docker build`/`compose` stay allowed. The check
  parses the shell command, so chained (`a && git push`), wrapped (`env`,
  `nice`, `timeout`), nested (`bash -c '...'`, `eval`, `$(...)`), and runner
  (`npx`, `uvx`, `uv run`, `poetry run`, `pnpm exec`, `python -m`, `xargs`,
  `find -exec`) forms are caught, as are options before the subcommand
  (`terraform -chdir=infra apply`, `helm -n prod upgrade`,
  `cargo +nightly publish`). The full list is `BLOCKED_COMMANDS` in
  `crates/jcode-app-core/src/jev_loop/config.rs`.
- Each helper attempt has a tool-call cap and an estimated spending cap. When
  either is hit, the attempt is stopped mid-turn and reported as a failure.
  The planner's read-only sessions have fixed tool-call caps too: judge 8,
  revise 15, and review 25 match the prototype's `max_turns`; the plan cap
  of 40 is new (the prototype's planner had none). When a read-only session
  reaches its cap, the tool calls in its next 2 responses are refused with
  "reply now with your answer" instead of ending the turn, so a judge that
  ran out of reads still returns its decision. The grace counts responses,
  not calls, so a response that asks for several reads at once cannot use it
  up. A tool call in a later response stops the session.
- Every session is told to leave changes uncommitted (no commits, branches,
  tags, stashes, or git config changes). The prototype got this from Claude
  Code's system prompt; here it is stated explicitly, because the loop's
  "review with `git diff`, undo with git" model depends on it.
- If Jev errors, times out, or returns an answer that does not validate (a
  missing answer, the wrong answer type, a probability outside 0 to 1), a
  review goes to Opus instead, and a file pick suggests nothing.

## Differences from the prototype

- The prototype asked Jev "done, retry, or escalate?" after every attempt,
  with the helper's own `check_passed` in hand. In practice Jev's answer
  matched "done if the helper said it passed, else escalate" in 49 of 50
  forks. The port runs the check itself, lets rules settle failures, and
  asks Jev the question rules cannot answer: does a passing change meet
  `done_when`?
- Jev file picks are slice 3 of the prototype's plan, which it never built.
- Claude sessions run on jcode's agent runtime instead of the Claude Agent
  SDK, so they use jcode's tools (`read`, `agentgrep`, `edit`, `bash`, ...).
  The planner, judge, and reviser get read-only tools, the reviewer can also
  run commands, and helpers can edit.
- The SDK's JSON-schema output mode is replaced by asking for a final JSON
  object and validating it. A missing or malformed reply falls back exactly as
  before: a failed helper report, an `escalate` judgment, or a `stop` revision.
- Sessions use a loop-specific system prompt instead of jcode's default one,
  so the target repo's `AGENTS.md` is not injected. This matches the Agent SDK,
  which does not load project settings unless asked. Helpers still read files
  in the repo when a step needs them.
- The Fable advisor is not ported. The prototype's `advisorModel` setting
  turned on Anthropic's server-side advisor tool through Claude Code. jcode's
  Anthropic client sends only its own tools and skips server-tool blocks in
  responses, so the advisor needs provider work first: send the tool and its
  beta header, round-trip `advisor_tool_result` blocks, resume `pause_turn`,
  and price advisor usage for the spending cap. To give the planner Fable
  instead, set `JCODE_JEV_LOOP_PLANNER_MODEL=claude-fable-5-1` (2.5 times the
  per-token price of Opus 5.5).
- Costs are estimated from token usage at public per-token prices. On a
  subscription (OAuth) route nothing is billed per token, but the numbers are
  still the right way to compare a Jev run against a `--no-jev` baseline.
- Every planner, helper, judge, reviser, and reviewer call is a jcode
  session titled `jev-loop <role>`, saved with its full transcript, so you
  can inspect exactly what each one did (for example with
  `jcode --resume <session>`). They are marked as internal sessions, like
  ambient cycles: hidden from the session picker until you show test
  sessions, and left out of the model-usage history.

## Stopping a run

Press Ctrl+C, or send the loop SIGTERM (`kill <pid>`), to stop a run. The
loop then stops everything it started, not just itself: it cancels every
session's turn, stops each helper's commands (the bash tool runs each one in
a session of its own, which the terminal's Ctrl+C never reaches), and kills
its own check run (a process group of its own). It prints
`jev-loop: Ctrl+C received, stopped the run and N command(s) it had started.`
and exits `130` (Ctrl+C) or `143` (SIGTERM).

The same cleanup runs when a run ends normally, and whenever one helper,
judge, or reviewer session ends, any command it left running in the
background (for example a test suite that outlived its foreground time
limit) is stopped with it.

Measured on Linux with the real binary and a real helper command (a local
stand-in model made the helper start a long-running command through the bash
tool): before this, the loop exited on Ctrl+C or `kill` while the helper's
command kept running and writing to disk; now both stop.

`kill -KILL` cannot be caught, so after SIGKILL the loop can clean nothing up.
Helper commands then keep running; stop them by their session or process
group, or use SIGTERM first.

## Safety note

Helpers can edit files and run shell commands in the repo without asking.
That is what makes the loop autonomous. Only point it at a repo you are happy
for it to change, keep it committed, and check `git diff` before you keep
anything. jcode's destructive-command gate still runs inside the bash tool as
a second layer behind the loop's own blocked-command check.

The blocked list reads the command line only, so it cannot see what a
script, a `make` target, `python -c`, or an SDK such as `boto3` does once it
runs. To cover that, every loop shell command also runs without this
machine's cloud and GitHub logins: the AWS, GitHub CLI, kubectl, gcloud, and
Azure tools are pointed at empty or missing configuration and their
credential variables are unset. A script that reaches for AWS or `gh` finds
no login. Git gets no password over HTTPS either: every configured credential
helper (`gh`, `store`, `cache`, Git Credential Manager) and askpass program is
switched off for the command, and git fails instead of prompting. Public
fetches and local git still work.
Values a command sets itself (`AWS_ACCESS_KEY_ID=test pytest`, as used for
local test stacks) still apply.

This does not cover everything. Database clients with a URL or password in
the repo or environment (`psql "$DATABASE_URL"`), `curl` with a token, SSH
keys or a running SSH agent, a token written into git configuration itself
(a remote URL with `user:token@`, `url.<base>.insteadOf`, or
`http.extraHeader`), and registry tokens in `~/.npmrc`, `~/.pypirc`,
`~/.cargo/credentials.toml`, or `~/.docker/config.json` are still reachable
from a script. If the shell can reach production that way, run the loop in a
container, VM, or user account that cannot.
