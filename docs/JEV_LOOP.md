# jev-loop

`jcode jev-loop` is a coding-agent loop with a Jev fork layer, ported from the
`jev-loop` Python prototype (slices 1 and 2) into jcode itself.

Opus plans, Sonnet helpers do each step, and Jev settles the small decisions
in between. When Jev is confident ("sharp"), code acts on its answer. When it
isn't ("split"), Opus decides.

## What runs where

| Piece | Prototype | In jcode |
|---|---|---|
| Settings | `config.py` | `crates/jcode-app-core/src/jev_loop/config.rs` |
| The loop | `loop.py` | `jev_loop/engine.rs` |
| Claude calls | `claude_calls.py` (Claude Agent SDK) | `jev_loop/claude.rs` (jcode's own agent runtime) |
| Fork layer | `jev_layer.py` (`typesafe-sdk`) | `jev_loop/fork.rs` (jcode's native Jev client) |
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

Baseline for comparison (every fork goes to Opus, Jev is never called):

```bash
jcode jev-loop --repo ~/code/myproject --task "..." --no-jev
```

Run both on the same task (reset the repo between runs) and compare the cost
lines in the summary. The exit code is `0` when every step finished and `2`
when the run stopped for you to decide.

## Reading the fork log

Every fork is appended to `~/.jcode/jev-loop/forks.jsonl` (one JSON line per
fork; set `JCODE_JEV_LOOP_FORK_LOG` to change it). It lives outside the
target repo so the loop never dirties the tree it is editing. The fields that
matter:

- `answer` and `confidence`: what Jev said and how sure it was
- `route`: `sharp` (code acted) or `split` (Opus decided)
- `final_decision` and `decided_by`: what actually happened, and who decided
  (`jev`, `opus`, or `rule`)
- `note`: when a rule overruled Jev, or Jev was unreachable

Tuning the threshold: look at split forks where Opus agreed with Jev's answer.
If Opus keeps agreeing at, say, 0.7, lower the threshold toward 0.7 and save
those Opus calls. If a sharp answer ever turned out wrong, raise it.

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
| `JCODE_JEV_LOOP_STEP_BUDGET_USD` | `2.00` |
| `JCODE_JEV_LOOP_FORK_LOG` | `~/.jcode/jev-loop/forks.jsonl` |

Invalid values are rejected at startup instead of being ignored.

## Rules that live in code (not in prompts)

- A step is never accepted as done if its check failed. If Jev says done,
  the fork goes to Opus instead; if Opus says done, code turns it into a
  retry. Both count as rule overrides. (The prototype's code only enforced
  this against Jev; the port enforces the README's rule for both.)
- After `MAX_ATTEMPTS_PER_STEP` tries, a step is escalated no matter what.
- Opus may rewrite a stuck step once, then the run stops for you to decide.
- Blocked commands (`git push`, `git reset --hard`, `git clean`, `rm -rf`,
  `sudo`) are refused for every session. The check parses the shell command,
  so chained (`a && git push`), wrapped (`env`, `nice`, `timeout`), and nested
  (`bash -c '...'`, `eval`) forms are caught too.
- Each helper attempt has a tool-call cap and an estimated spending cap. When
  either is hit, the attempt is stopped mid-turn and reported as a failure.
- If Jev errors, times out, or returns an answer that does not validate (an
  unknown choice, probabilities that do not sum to one, a choice that
  disagrees with its own probabilities), that fork goes to Opus instead.

## Differences from the prototype

- Claude sessions run on jcode's agent runtime instead of the Claude Agent
  SDK, so they use jcode's tools (`read`, `agentgrep`, `edit`, `bash`, ...).
  The planner, judge, and reviser get read-only tools, the reviewer can also
  run commands, and helpers can edit.
- The SDK's JSON-schema output mode is replaced by asking for a final JSON
  object and validating it. A missing or malformed reply falls back exactly as
  before: a failed helper report, an `escalate` judgment, or a `stop` revision.
- The Fable advisor setting is not ported: jcode has no advisor hook for
  per-session settings.
- Costs are estimated from token usage at public per-token prices. On a
  subscription (OAuth) route nothing is billed per token, but the numbers are
  still the right way to compare a Jev run against a `--no-jev` baseline.
- Every planner, helper, judge, reviser, and reviewer call is an ordinary
  jcode session titled `jev-loop <role>`, saved with its full transcript, so
  you can inspect exactly what each one did (for example with
  `jcode --resume <session>`).

## Safety note

Helpers can edit files and run shell commands in the repo without asking.
That is what makes the loop autonomous. Only point it at a repo you are happy
for it to change, keep it committed, and check `git diff` before you keep
anything. jcode's destructive-command gate still runs inside the bash tool as
a second layer behind the loop's own blocked-command check.
