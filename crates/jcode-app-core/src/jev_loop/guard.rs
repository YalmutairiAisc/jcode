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
    budget_usd: Option<f64>,
    blocked_commands: &'static [&'static [&'static str]],
    tool_calls: AtomicU32,
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
            budget_usd,
            blocked_commands,
            tool_calls: AtomicU32::new(0),
            spent_usd: Mutex::new(0.0),
            stop_reason: Mutex::new(None),
            cancel: Mutex::new(None),
        })
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
    /// the session actually attempted.
    fn admit(&self, tool_name: &str, input: &Value) -> Result<(), String> {
        if let Some(reason) = self.stop_reason() {
            return Err(format!("This session was stopped: {reason}."));
        }
        let used = self.tool_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if used > self.max_tool_calls {
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

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        if let Err(refusal) = self.guard.admit(self.inner.name(), &input) {
            anyhow::bail!(refusal);
        }
        self.inner.execute(input, ctx).await
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
        let words = strip_command_prefixes(&words);
        for pattern in blocked {
            if matches_pattern(words, pattern) {
                return Some(pattern.join(" "));
            }
        }
        if depth < 3
            && let Some(script) = nested_script(words)
            && let Some(found) = blocked_command_at_depth(&script, blocked, depth + 1)
        {
            return Some(found);
        }
    }
    None
}

/// The script a shell wrapper runs: `bash -c '...'`, `sh -lc "..."`, `eval ...`.
fn nested_script(words: &[String]) -> Option<String> {
    let (program, args) = words.split_first()?;
    let program = program.rsplit('/').next().unwrap_or(program);
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
/// and every later pattern word appears among the arguments. For `rm` flags
/// (`-rf`), bundled or split flags (`-r -f`, `-fr`, `-Rf`) also match. For
/// `git`, options before the subcommand (`git -C dir push`) are skipped.
fn matches_pattern(words: &[String], pattern: &[&str]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let Some((first, rest)) = pattern.split_first() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    if program != *first {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    if *first == "rm" {
        return rest
            .iter()
            .all(|flag| rm_has_flags(args, flag.trim_start_matches('-')));
    }
    let args = if *first == "git" {
        skip_git_global_options(args)
    } else {
        args
    };
    // The subcommand must come first; later words may appear anywhere.
    let Some((sub, later)) = rest.split_first() else {
        return true;
    };
    args.first().is_some_and(|arg| arg == sub)
        && later
            .iter()
            .all(|word| args.iter().skip(1).any(|arg| arg == word))
}

fn skip_git_global_options(args: &[String]) -> &[String] {
    let mut rest = args;
    while let Some(first) = rest.first() {
        if matches!(first.as_str(), "-C" | "-c" | "--git-dir" | "--work-tree") {
            rest = rest.get(2..).unwrap_or(&[]);
        } else if first.starts_with('-') {
            rest = &rest[1..];
        } else {
            break;
        }
    }
    rest
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
