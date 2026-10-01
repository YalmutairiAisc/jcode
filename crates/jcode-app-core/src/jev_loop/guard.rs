//! Rules that live in code, not in prompts: blocked commands, a per-session
//! tool-call cap, and a per-attempt spending cap.
//!
//! Each loop session gets its own tool registry in which every tool is
//! wrapped by [`GuardedTool`]. The wrapper runs before the real tool, so a
//! model cannot talk its way past it, and it is scoped to that registry, so
//! nothing here leaks into other jcode sessions. On the first violation the
//! session's turn is stopped through the agent's own cancel signal.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::tool::{Tool, ToolContext, ToolOutput};

/// Per-session limits and the reason a session was stopped, if it was.
pub struct SessionGuard {
    max_tool_calls: u32,
    /// Model responses whose over-cap tool calls are refused, without
    /// stopping the turn, so the model can still give its answer (see
    /// [`SessionGuard::with_answer_grace`]). Zero means the call over the cap
    /// stops the session at once.
    answer_grace: u32,
    budget_usd: Option<f64>,
    blocked_commands: &'static [&'static [&'static str]],
    tool_calls: AtomicU32,
    /// Responses (by message id) that have had an over-cap call refused.
    grace_responses: Mutex<Vec<String>>,
    spent_usd: Mutex<f64>,
    stop_reason: Mutex<Option<String>>,
    cancel: Mutex<Option<crate::agent::InterruptSignal>>,
}

impl SessionGuard {
    pub fn new(
        max_tool_calls: u32,
        budget_usd: Option<f64>,
        blocked_commands: &'static [&'static [&'static str]],
    ) -> Arc<Self> {
        Arc::new(Self {
            max_tool_calls,
            answer_grace: 0,
            budget_usd,
            blocked_commands,
            tool_calls: AtomicU32::new(0),
            grace_responses: Mutex::new(Vec::new()),
            spent_usd: Mutex::new(0.0),
            stop_reason: Mutex::new(None),
            cancel: Mutex::new(None),
        })
    }

    /// Like [`SessionGuard::new`], but once the cap is reached, tool calls
    /// from the next `grace` model responses are refused with an instruction
    /// to answer now, instead of stopping the session. A judge, reviser,
    /// planner, or reviewer that runs out of tool calls then still returns
    /// its decision; stopping it outright left the loop with no answer at
    /// all. The grace counts responses, not calls, so a response that
    /// batches several reads cannot use it all up before the model has seen
    /// a single refusal. A tool call from a response after the grace stops
    /// the session.
    pub fn with_answer_grace(
        max_tool_calls: u32,
        grace: u32,
        budget_usd: Option<f64>,
        blocked_commands: &'static [&'static [&'static str]],
    ) -> Arc<Self> {
        let mut guard = Self::new(max_tool_calls, budget_usd, blocked_commands);
        if let Some(guard) = Arc::get_mut(&mut guard) {
            guard.answer_grace = grace;
        }
        guard
    }

    /// Wire the owning agent's cancel signal so a violation ends the turn.
    pub fn attach_cancel(&self, signal: crate::agent::InterruptSignal) {
        *lock(&self.cancel) = Some(signal);
    }

    pub fn tool_calls(&self) -> u32 {
        self.tool_calls.load(Ordering::SeqCst)
    }

    pub fn stop_reason(&self) -> Option<String> {
        lock(&self.stop_reason).clone()
    }

    /// Record spend observed so far; stops the session once over budget.
    pub fn record_spend(&self, spent_usd: f64) {
        *lock(&self.spent_usd) = spent_usd;
        if let Some(budget) = self.budget_usd
            && spent_usd > budget
        {
            self.stop(format!(
                "spending cap reached (${} of ${} for this attempt)",
                usd(spent_usd),
                usd(budget)
            ));
        }
    }

    fn stop(&self, reason: String) {
        let mut slot = lock(&self.stop_reason);
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        if let Some(signal) = lock(&self.cancel).as_ref() {
            signal.fire();
        }
    }

    /// Admit one tool call, or explain why it may not run. Only calls that
    /// get past the stop check are counted, so `tool_calls` reports the calls
    /// the session actually attempted. `response_id` identifies the model
    /// response the call came from (the agent's assistant message id).
    fn admit(&self, tool_name: &str, input: &Value, response_id: &str) -> Result<(), String> {
        if let Some(reason) = self.stop_reason() {
            return Err(format!("This session was stopped: {reason}."));
        }
        let used = self.tool_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if used > self.max_tool_calls {
            if self.grace_allows(response_id) {
                return Err(format!(
                    "Tool-call limit reached ({} calls). Do not call any more tools: \
                     reply now with your answer in the required format, using what \
                     you have already read.",
                    self.max_tool_calls
                ));
            }
            let reason = format!("tool-call limit reached ({} calls)", self.max_tool_calls);
            self.stop(reason.clone());
            return Err(format!("This session was stopped: {reason}."));
        }
        if tool_name == "bash"
            && let Some(command) = input.get("command").and_then(Value::as_str)
            && let Some(blocked) = blocked_command(command, self.blocked_commands)
        {
            return Err(format!(
                "Blocked by the jev-loop policy: `{blocked}` is never allowed in this loop. \
                 Do not retry it; report a blocker if the step cannot be done without it."
            ));
        }
        Ok(())
    }

    /// Whether an over-cap call from `response_id` gets a refusal that lets
    /// the turn continue. Every call in a response already in the grace
    /// does; a new response does only while fewer than `answer_grace`
    /// responses have used it.
    fn grace_allows(&self, response_id: &str) -> bool {
        if self.answer_grace == 0 {
            return false;
        }
        let mut seen = lock(&self.grace_responses);
        if seen.iter().any(|id| id == response_id) {
            return true;
        }
        if seen.len() < self.answer_grace as usize {
            seen.push(response_id.to_string());
            return true;
        }
        false
    }
}

/// Dollars with cents, or more digits for sub-cent amounts so a tiny budget
/// never reads as `$0.00 of $0.00`.
fn usd(amount: f64) -> String {
    if amount.abs() >= 0.01 || amount == 0.0 {
        format!("{amount:.2}")
    } else {
        format!("{amount:.4}")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Wraps a real tool with the session's guard.
pub struct GuardedTool {
    inner: Arc<dyn Tool>,
    guard: Arc<SessionGuard>,
}

impl GuardedTool {
    pub fn new(inner: Arc<dyn Tool>, guard: Arc<SessionGuard>) -> Self {
        Self { inner, guard }
    }
}

#[async_trait]
impl Tool for GuardedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn to_definition(&self) -> jcode_message_types::ToolDefinition {
        self.inner.to_definition()
    }

    async fn execute(&self, mut input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        if let Err(refusal) = self.guard.admit(self.inner.name(), &input, &ctx.message_id) {
            anyhow::bail!(refusal);
        }
        if self.inner.name() != "bash" {
            return self.inner.execute(input, ctx).await;
        }
        // The bash tool reads the command's first words for its hints, so
        // take the hint from the helper's own command before the isolation
        // lines go in front of it.
        let hint = input
            .get("command")
            .and_then(Value::as_str)
            .and_then(crate::tool::bash::file_edit_hint);
        hide_credentials(&mut input);
        let mut output = self.inner.execute(input, ctx).await?;
        if let Some(hint) = hint
            && !output.output.contains(hint)
        {
            output.output.push_str("\n\n");
            output.output.push_str(hint);
        }
        Ok(output)
    }
}

/// Shell lines run ahead of every loop `bash` command. They point the AWS,
/// GitHub, Kubernetes, Google Cloud, and Azure tools and SDKs at empty
/// configuration and drop credential variables, so a program the blocked list
/// cannot see (`bash deploy.sh`, `boto3`, `terraform` started from Python)
/// finds no login to act with. Git gets no password either: an empty
/// `credential.helper` entry (git's documented way to reset the list) turns
/// off every configured helper (`store`, `cache`, Git Credential Manager,
/// `gh`), the askpass programs are dropped, and git fails instead of
/// prompting. The two git entries are added after any `GIT_CONFIG_COUNT`
/// entries the parent already set, so those keep working. Settings a parent
/// passed down with `git -c` (`GIT_CONFIG_PARAMETERS`) are dropped, because
/// git applies them after these entries and they could turn a helper back on.
const CREDENTIAL_ISOLATION: &str = "\
unset AWS_PROFILE AWS_DEFAULT_PROFILE AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY \
AWS_SESSION_TOKEN AWS_SECURITY_TOKEN AWS_WEB_IDENTITY_TOKEN_FILE AWS_ROLE_ARN \
AWS_CONTAINER_CREDENTIALS_FULL_URI AWS_CONTAINER_CREDENTIALS_RELATIVE_URI \
AWS_CONTAINER_AUTHORIZATION_TOKEN GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN \
GITHUB_ENTERPRISE_TOKEN GOOGLE_APPLICATION_CREDENTIALS GIT_ASKPASS SSH_ASKPASS \
GIT_CONFIG_PARAMETERS
export AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null \
AWS_EC2_METADATA_DISABLED=true GH_CONFIG_DIR=/dev/null/gh KUBECONFIG=/dev/null \
CLOUDSDK_CONFIG=/dev/null/gcloud AZURE_CONFIG_DIR=/dev/null/azure GIT_TERMINAL_PROMPT=0
__jev_n=${GIT_CONFIG_COUNT:-0}
export \"GIT_CONFIG_KEY_$__jev_n=credential.helper\" \"GIT_CONFIG_VALUE_$__jev_n=\" \
\"GIT_CONFIG_KEY_$((__jev_n + 1))=core.askPass\" \"GIT_CONFIG_VALUE_$((__jev_n + 1))=\" \
GIT_CONFIG_COUNT=$((__jev_n + 2))
unset __jev_n";

/// Run a loop shell command without this machine's cloud and GitHub logins.
/// Values the command sets itself (`AWS_ACCESS_KEY_ID=test pytest`) still
/// apply, because they come after these lines. Unix shells only: on Windows
/// the bash tool runs `cmd.exe`, which this syntax does not fit.
pub(crate) fn hide_credentials(input: &mut Value) {
    if cfg!(windows) {
        return;
    }
    if let Some(command) = input.get("command").and_then(Value::as_str) {
        let isolated = format!("{CREDENTIAL_ISOLATION}\n{command}");
        input["command"] = Value::String(isolated);
    }
}

/// Return the blocked word sequence a shell command contains, if any. Every
/// `&&`, `||`, `;`, `|`, and newline-separated segment is checked, after
/// stripping leading environment assignments and wrappers like `env`, `nice`,
/// `timeout`, `command`, `nohup`, and `time`. Scripts run through
/// `bash -c`, `sh -lc`, or `eval` are checked too. Parsing is shared with
/// jcode's destructive-command gate, so quoting is resolved the same way.
/// This is a policy filter, not a sandbox: jcode's own destructive-command
/// gate still runs inside the bash tool as a second layer.
pub fn blocked_command(
    command: &str,
    blocked: &'static [&'static [&'static str]],
) -> Option<String> {
    blocked_command_at_depth(command, blocked, 0)
}

fn blocked_command_at_depth(
    command: &str,
    blocked: &'static [&'static [&'static str]],
    depth: u8,
) -> Option<String> {
    if depth < 3 {
        // `$(...)` and backticks run their contents first, wherever they
        // appear (`echo $(git push)`, `x=`git push``), so check them too.
        for script in substitutions(command) {
            if let Some(found) = blocked_command_at_depth(&script, blocked, depth + 1) {
                return Some(found);
            }
        }
    }
    for segment in jcode_command_risk::split_segments(command) {
        let words: Vec<String> = segment
            .iter()
            .filter(|token| !token.is_operator)
            .map(|token| token.text.clone())
            .collect();
        let words = strip_command_prefixes(&words).to_vec();
        // `npx vercel`, `uv run aws ...`, `python -m twine upload`: a runner
        // starts another program, so that program is checked as well.
        let mut views = vec![words];
        while views.len() < 4
            && let Some(target) = views.last().and_then(|view| runner_target(view))
        {
            views.push(strip_command_prefixes(&target).to_vec());
        }
        for view in &views {
            if let Some(pattern) = blocked
                .iter()
                .find(|pattern| matches_pattern(view, pattern))
            {
                return Some(pattern.join(" "));
            }
            if depth < 3
                && let Some(script) = nested_script(view)
                && let Some(found) = blocked_command_at_depth(&script, blocked, depth + 1)
            {
                return Some(found);
            }
        }
    }
    None
}

/// The script a shell wrapper runs: `bash -c '...'`, `sh -lc "..."`, `eval ...`.
fn nested_script(words: &[String]) -> Option<String> {
    let (program, args) = words.split_first()?;
    let program = program_name(program);
    if program == "eval" {
        return Some(args.join(" "));
    }
    if !matches!(program, "bash" | "sh" | "zsh" | "dash" | "ksh") {
        return None;
    }
    let flag = args
        .iter()
        .position(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c'))?;
    args.get(flag + 1).cloned()
}

/// Bodies of `$(...)` and `` `...` `` command substitutions (outermost level).
fn substitutions(command: &str) -> Vec<String> {
    let mut found = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '`' {
            if let Some(close) = chars[index + 1..].iter().position(|&c| c == '`') {
                found.push(chars[index + 1..index + 1 + close].iter().collect());
                index += close + 2;
                continue;
            }
        } else if chars[index] == '$' && chars.get(index + 1) == Some(&'(') {
            let mut depth = 0usize;
            for (offset, &c) in chars[index + 1..].iter().enumerate() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            let body = &chars[index + 2..index + 1 + offset];
                            found.push(body.iter().collect());
                            index += offset + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        index += 1;
    }
    found
}

fn strip_command_prefixes(words: &[String]) -> &[String] {
    let mut rest = words;
    while let Some(first) = rest.first() {
        if is_env_assignment(first) {
            rest = &rest[1..];
            continue;
        }
        let wrapper = first.rsplit('/').next().unwrap_or(first);
        // Flags of each wrapper that consume the following word.
        let value_flags: &[&str] = match wrapper {
            "env" => &["-u", "--unset", "-C", "--chdir"],
            "nice" => &["-n", "--adjustment"],
            "timeout" => &["-s", "--signal", "-k", "--kill-after"],
            "command" | "builtin" | "nohup" | "time" | "exec" => &[],
            _ => return rest,
        };
        rest = &rest[1..];
        while let Some(flag) = rest.first().filter(|word| word.starts_with('-')) {
            let skip = if value_flags.contains(&flag.as_str()) {
                2
            } else {
                1
            };
            rest = rest.get(skip..).unwrap_or(&[]);
        }
        if wrapper == "timeout" && !rest.is_empty() {
            // The duration operand, e.g. `timeout 10 git push`.
            rest = &rest[1..];
        }
    }
    rest
}

fn is_env_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// A pattern matches when the command's program is the pattern's first word
/// and, for a longer pattern, the program's subcommand is the pattern's
/// second word and every later pattern word appears after it. Options before
/// the subcommand are skipped (`git -C dir push`, `helm -n prod upgrade`,
/// `terraform -chdir=infra apply`, `cargo +nightly publish`). For `rm` flags
/// (`-rf`), bundled or split flags (`-r -f`, `-fr`, `-Rf`) also match.
fn matches_pattern(words: &[String], pattern: &[&str]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let Some((first, rest)) = pattern.split_first() else {
        return false;
    };
    let program = program_name(program);
    if program != *first {
        return false;
    }
    let Some((sub, later)) = rest.split_first() else {
        return true;
    };
    if *first == "rm" {
        return rest
            .iter()
            .all(|flag| rm_has_flags(args, flag.trim_start_matches('-')));
    }
    let Some(index) = first_positional(args, global_value_options(program)) else {
        return false;
    };
    args[index] == *sub
        && later
            .iter()
            .all(|word| args[index + 1..].iter().any(|arg| arg == word))
}

fn program_name(word: &str) -> &str {
    let base = word.rsplit('/').next().unwrap_or(word);
    // Package runners accept a version: `npx vercel@latest`, `uvx twine@6`.
    match base.find('@') {
        Some(at) if at > 0 => &base[..at],
        _ => base,
    }
}

/// Index of the first word that is neither an option nor an option's value.
/// Options named in `value_options` take the next word as their value; any
/// other option is a flag, and `--option=value` is one word. `+toolchain`
/// counts as a flag. The word after `--` is positional whatever it is.
fn first_positional(args: &[String], value_options: &[&str]) -> Option<usize> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return (index + 1 < args.len()).then_some(index + 1);
        }
        if arg.len() > 1 && (arg.starts_with('-') || arg.starts_with('+')) {
            index += if value_options.contains(&arg.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        return Some(index);
    }
    None
}

/// Options that take a separate value before a program's subcommand. Only
/// programs that are blocked by subcommand, or that start runners, need
/// entries.
fn global_value_options(program: &str) -> &'static [&'static str] {
    match program {
        "git" => &["-C", "-c", "--git-dir", "--work-tree", "--namespace"],
        "docker" => &[
            "-c",
            "--context",
            "-H",
            "--host",
            "--config",
            "-l",
            "--log-level",
            "--tlscacert",
            "--tlscert",
            "--tlskey",
        ],
        "docker-compose" => &["-f", "--file", "-p", "--project-name", "--env-file"],
        "podman" => &["-c", "--connection", "--url", "--identity", "--log-level"],
        "helm" => &["-n", "--namespace", "--kube-context", "--kubeconfig"],
        "npm" => &[
            "-w",
            "--workspace",
            "--prefix",
            "--registry",
            "--userconfig",
            "--loglevel",
            "--cache",
            "--tag",
            "--access",
            "--otp",
        ],
        "pnpm" => &[
            "-C",
            "--dir",
            "-F",
            "--filter",
            "--registry",
            "--tag",
            "--access",
        ],
        "yarn" => &["--cwd", "--registry"],
        "cargo" => &["--config", "-Z", "-C", "--color"],
        "uv" => &[
            "--directory",
            "--project",
            "--config-file",
            "--cache-dir",
            "--color",
        ],
        "poetry" => &["-C", "--directory", "-P", "--project"],
        "fly" | "flyctl" => &["-a", "--app", "-c", "--config"],
        "wrangler" => &["-c", "--config", "-e", "--env", "--cwd"],
        "firebase" => &["-P", "--project"],
        "cdk" => &["-a", "--app", "-c", "--context", "--profile"],
        "pulumi" => &["-C", "--cwd"],
        _ => &[],
    }
}

/// Programs that start another program: the runner, the words that select
/// its run mode, and the runner's options that take a separate value.
const RUNNERS: &[(&str, &[&str], &[&str])] = &[
    ("npx", &[], &["-p", "--package"]),
    ("pnpx", &[], &["-p", "--package"]),
    ("bunx", &[], &["-p", "--package"]),
    (
        "uvx",
        &[],
        &["--from", "-w", "--with", "-p", "--python", "--env-file"],
    ),
    (
        "xargs",
        &[],
        &["-I", "-n", "-P", "-L", "-d", "-E", "-s", "-a"],
    ),
    ("npm", &["exec"], &["-p", "--package"]),
    ("npm", &["x"], &["-p", "--package"]),
    ("pnpm", &["exec"], &[]),
    ("pnpm", &["dlx"], &["-p", "--package"]),
    ("yarn", &["dlx"], &["-p", "--package"]),
    ("yarn", &["exec"], &[]),
    ("bun", &["x"], &["-p", "--package"]),
    (
        "uv",
        &["run"],
        &[
            "-w",
            "--with",
            "--with-editable",
            "--with-requirements",
            "--package",
            "--extra",
            "--group",
            "--only-group",
            "--env-file",
            "--index",
            "--default-index",
            "-i",
            "--index-url",
            "--extra-index-url",
            "-p",
            "--python",
            "-C",
            "--config-setting",
            "--directory",
            "--project",
        ],
    ),
    (
        "uv",
        &["tool", "run"],
        &[
            "--from",
            "-w",
            "--with",
            "--with-editable",
            "--with-requirements",
            "-c",
            "--constraints",
            "--env-file",
            "--index",
            "-i",
            "--index-url",
            "-p",
            "--python",
            "--directory",
            "--project",
        ],
    ),
    ("pipx", &["run"], &["--spec", "--python"]),
    ("poetry", &["run"], &[]),
    ("pdm", &["run"], &[]),
    ("hatch", &["run"], &[]),
    ("bundle", &["exec"], &[]),
];

/// The program a runner starts, with its arguments: `npx vercel`,
/// `uv run -- aws s3 ls`, `python -m twine upload`, `xargs -n 1 aws ...`,
/// `find . -exec twine upload {} +`.
fn runner_target(words: &[String]) -> Option<Vec<String>> {
    let (program, args) = words.split_first()?;
    let program = program_name(program);
    if program == "python" || program.starts_with("python3") {
        return python_module(args);
    }
    if program == "find" {
        let start = args
            .iter()
            .position(|arg| matches!(arg.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))?;
        let target = args[start + 1..]
            .iter()
            .take_while(|arg| !matches!(arg.as_str(), ";" | "+"))
            .cloned()
            .collect();
        return Some(target);
    }
    for (runner, verbs, value_options) in RUNNERS {
        if program != *runner {
            continue;
        }
        let mut rest = args;
        if !verbs.is_empty() {
            let Some(at) = first_positional(rest, global_value_options(program)) else {
                continue;
            };
            let selected = rest.len() >= at + verbs.len()
                && rest[at..]
                    .iter()
                    .zip(verbs.iter())
                    .all(|(arg, verb)| arg == verb);
            if !selected {
                continue;
            }
            rest = &rest[at + verbs.len()..];
        }
        if let Some(start) = first_positional(rest, value_options) {
            return Some(rest[start..].to_vec());
        }
    }
    None
}

/// `python -m module args` gives the module and its arguments. A script or
/// `-c` code is not followed: the guard cannot see what it runs.
fn python_module(args: &[String]) -> Option<Vec<String>> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-m" => return args.get(index + 1..).map(<[String]>::to_vec),
            "-W" | "-X" => index += 2,
            flag if flag.starts_with("-m") => {
                let mut target = vec![flag["-m".len()..].to_string()];
                target.extend_from_slice(&args[index + 1..]);
                return Some(target);
            }
            flag if flag.len() > 1 && flag.starts_with('-') && !flag.starts_with("-c") => {
                index += 1;
            }
            _ => return None,
        }
    }
    None
}

fn rm_has_flags(args: &[String], wanted: &str) -> bool {
    let mut seen = String::new();
    for arg in args {
        match arg.as_str() {
            "--recursive" => seen.push('r'),
            "--force" => seen.push('f'),
            flag if flag.starts_with('-') && !flag.starts_with("--") => {
                seen.push_str(&flag[1..].to_ascii_lowercase());
            }
            _ => {}
        }
    }
    wanted.chars().all(|c| seen.contains(c))
}
