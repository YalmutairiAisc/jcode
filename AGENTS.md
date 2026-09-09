# Repository Guidelines

## Development Workflow

- **Stay on your own branch** - Do not take, cherry-pick, merge, or copy code from other
  people's or other agents' branches unless the source branch belongs to a repository
  maintainer and the user explicitly asks you to integrate it. Only work from your branch
  and its base (e.g. `main`) otherwise. Never integrate branches owned by non-maintainers
  or other agents yourself; tell the user and let them decide how to proceed.

## Install Notes
- `~/.local/bin/jcode` is the launcher symlink used from `PATH`.
- `~/.jcode/builds/current/jcode` is the active local/source-build channel; self-dev builds and `scripts/install_release.sh` point the launcher here.
- `~/.jcode/builds/stable/jcode` is the stable release channel; `scripts/install.sh` installs this and points the launcher here.
- `~/.jcode/builds/versions/<version>/jcode` stores immutable binaries.
- `~/.jcode/builds/canary/jcode` still exists for canary/testing flows, but it is not the primary self-dev install path.
- On Windows, the equivalents are `%LOCALAPPDATA%\\jcode\\bin\\jcode.exe` for the launcher, `%LOCALAPPDATA%\\jcode\\builds\\stable\\jcode.exe` for stable, and `%LOCALAPPDATA%\\jcode\\builds\\versions\\<version>\\jcode.exe` for immutable installs; `scripts/install.ps1` currently installs the stable channel.
- Ensure `~/.local/bin` is **before** `~/.cargo/bin` in `PATH`.

## Verifying a change at runtime

`cargo build` alone proves nothing about behavior. `jcode run` and interactive
sessions are served by the long-lived daemon at
`~/.jcode/builds/shared-server/jcode`, which is a symlink into
`~/.jcode/builds/versions/<version>/`. Until that symlink is repointed and the
daemon restarted (`jcode self-dev --build`), a freshly built binary is inert and
every runtime check silently measures the old code.

To test a change without disturbing the shared daemon or the caller's session,
run your build against its own socket:

```bash
cargo build --profile selfdev
./target/selfdev/jcode run --no-update --socket /run/user/1000/jcode-mytest.sock '<prompt>'
```

Two things that waste time otherwise:

- `crate::logging::info` writes to a log file, not stderr, so instrumenting a
  code path with it produces no visible output under `--trace`. Use `eprintln!`
  for throwaway diagnostics and delete it before committing.
- Confirm which binary you are actually inspecting. `strings` on
  `builds/shared-server/jcode` reads a 70-byte symlink, not a program; resolve it
  with `readlink -f` first.

## The two test locks: always env, then render

`jcode-tui` tests serialize on two process-global mutexes:

- `jcode_base::storage::lock_test_env()` guards `JCODE_HOME` and other env vars
- `jcode_tui::tui::ui::render_state_test_lock()` guards global render state

**Always take env first.** `create_test_app` takes the render lock internally, so
a test that holds render and *then* reaches for env deadlocks against the many
tests that hold env and then build an app. Neither lock is reentrant.

Watch for the indirect form: `with_temp_jcode_home`, `with_reasoning_current_home`
and `with_ssh_remote_test_home` all take the env lock inside, so calling one while
holding a render lock is the same inversion. Usually you do not need an explicit
render lock at all, because `create_test_app` already takes it.

`tui::ui::tests::test_locks_are_always_taken_env_before_render` enforces this by
scanning the source; it names the file, line, and offending call. It is a static
check because the deadlock is timing-dependent: the isolated two-test repro took
about five runs to wedge, so a green run proves very little.

### If the suite hangs anyway

Two inverted tests used to wedge every worker at any thread count above 2, which
silently hid every test after the wedge. That is how a batch of real Windows
failures survived for months, and why the whole suite now runs in ~50s at the
default 16 threads. To diagnose a new one, dump the stacks of the hung process:

```bash
gdb -p <pid> --batch -ex "set pagination off" -ex "thread apply all bt 25"
```

Read it carefully: holding a mutex leaves no stack frame, so a thread that owns
one lock while blocking on the other looks identical to a thread that owns
nothing. "No thread holds either lock" is what an A-then-B/B-then-A cycle looks
like, not evidence of a leaked guard. Match each blocked test against the order
it takes the two locks.

## Known: the suite is not isolated under parallelism

`--test-threads=1` is green (2270 passed / 0 failed). At the default thread
count, 3-7 tests fail per run and **the set changes every run**. They pass
individually and serially. This is test isolation, not product behavior.

The cause is that env vars are process-global while ~800 tests read them
concurrently. `lock_test_env` only excludes other env-*mutating* tests, so a
test that perturbs the environment is invisible to every reader:

- `JCODE_HOME` moves where session and reload-context files are written, so
  `test_restore_session_*` writes into another test's temp dir
- `JCODE_SSH_REMOTE` makes `parse_dropped_paths` return `None`, so the
  drag-and-drop tests fail inside product code that never mentions the
  environment

`JCODE_HOME` alone is set at 75 sites across 21 files, only three of which go
through `with_temp_jcode_home`.

### What does not work

An `RwLock` with readers parked in a thread-local. It is the right *shape*
(readers do not exclude each other, writers exclude everyone) and it fixes the
isolated repro, 13/20 runs to 20/20. It deadlocks the full suite.

The flaw is that a parked guard has no scoped owner. libtest threads outlive
the tests that ran on them, so a read guard parked by `create_test_app` stays
held after that test finishes. A later writer blocks on it *while holding the
env mutex*, and every other test then piles up behind that mutex. Releasing the
guard in `lock_test_env` is not enough, since the blocking writer may be on a
different thread than the one still parking a guard. Using `try_read` instead
avoids the deadlock but silently declines the guard exactly when a writer holds
the lock, which is the only moment it was needed.

A working fix needs the read guard to be scoped to the test, which means
returning it to the test body rather than parking it. `create_test_app` returns
`App`, so that is a signature change across ~950 call sites, or a new
`create_test_app_guarded` adopted by the tests that need it.
