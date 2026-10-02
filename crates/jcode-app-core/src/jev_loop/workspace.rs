//! What the loop reads and runs in the repository itself, instead of
//! trusting a model's account of it.
//!
//! - [`Workspace::run_check`] runs a helper's check command, so a step's
//!   fate rests on what the loop saw. The run gets the protections a helper's
//!   own shell commands get: the loop's blocked-command list, no cloud or
//!   GitHub logins (`guard::hide_credentials`), jcode's destructive-command
//!   gate, a time limit, and the repository as its working directory.
//! - [`Workspace::snapshot`] and [`Workspace::diff_since`] show a judge the
//!   changes one step made, new files included.
//! - [`Workspace::candidates`] is the keyword search behind Jev's file picks.

use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::config;
use super::fork::tail;
use super::guard;
use super::pick::{self, Candidate};
use super::report::LoopCheck;

/// Characters of check output kept for judges and retry feedback.
pub const CHECK_OUTPUT_TAIL_CHARS: usize = 3000;
/// Files larger than this are named in a diff, not shown.
const MAX_SHOWN_FILE_BYTES: u64 = 256 * 1024;
/// Most changed files one snapshot records.
const MAX_SNAPSHOT_FILES: usize = 5000;
/// Lines shown to Jev for each candidate file.
const LINES_PER_CANDIDATE: usize = 3;
/// Smallest share of a diff budget a changed file gets when not all fit.
const MIN_SECTION_BYTES: usize = 1500;

/// The repository as the loop sees it. Mocked in tests.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Run a helper's check command the way the loop does (see the module docs).
    async fn run_check(&self, command: &str) -> LoopCheck;
    /// Every changed or new file right now.
    async fn snapshot(&self) -> Snapshot;
    /// The changes made since `start`, as text for a judge, in about
    /// `max_bytes`. Empty when nothing changed.
    async fn diff_since(&self, start: &Snapshot, max_bytes: usize) -> String;
    /// Files a keyword search ties to `text`, best first, at most `limit`.
    async fn candidates(&self, text: &str, limit: usize) -> Vec<Candidate>;
}

pub struct RepoWorkspace {
    repo: PathBuf,
    check_timeout: Duration,
}

impl RepoWorkspace {
    pub fn new(repo: PathBuf, check_timeout: Duration) -> Self {
        Self {
            repo,
            check_timeout,
        }
    }

    async fn section(
        &self,
        path: &str,
        before: Option<&FileState>,
        now: Option<&FileState>,
    ) -> (String, String) {
        if now.is_some_and(|state| state.untracked) {
            return (format!("new file {path}"), show_file(&self.repo.join(path)));
        }
        if now.is_none() && before.is_some_and(|state| state.untracked) {
            return (
                format!("removed {path}"),
                "(a file created earlier was deleted)".into(),
            );
        }
        let args = ["diff", "HEAD", "--no-color", "--no-ext-diff", "--", path];
        match git(&self.repo, &args).await {
            Ok(raw) if !raw.is_empty() => (
                format!("changed {path}"),
                String::from_utf8_lossy(&raw).into_owned(),
            ),
            Ok(_) => (
                format!("restored {path}"),
                "(back to the committed version)".into(),
            ),
            Err(error) => (format!("changed {path}"), format!("(no diff: {error:#})")),
        }
    }

    async fn files_containing(&self, word: &str) -> Vec<String> {
        let args = [
            "grep",
            "--untracked",
            "-I",
            "-i",
            "-F",
            "-l",
            "-z",
            "--no-color",
            "-e",
            word,
        ];
        match git(&self.repo, &args).await {
            Ok(raw) => split_nul(&raw),
            Err(error) => {
                crate::logging::warn(&format!("jev-loop: keyword search failed: {error:#}"));
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl Workspace for RepoWorkspace {
    async fn run_check(&self, command: &str) -> LoopCheck {
        let command = command.trim();
        if command.is_empty() {
            return LoopCheck::not_run("the helper named no check command");
        }
        if let Some(blocked) = guard::blocked_command(command, config::BLOCKED_COMMANDS) {
            return LoopCheck::not_run(format!("`{blocked}` is never allowed in this loop"));
        }
        run_check_in(&self.repo, command, self.check_timeout).await
    }

    async fn snapshot(&self) -> Snapshot {
        let args = ["status", "--porcelain=v1", "-z", "--untracked-files=all"];
        match git(&self.repo, &args).await {
            Ok(raw) => Snapshot::from_status(&self.repo, &raw),
            Err(error) => {
                crate::logging::warn(&format!("jev-loop: git status failed: {error:#}"));
                Snapshot::default()
            }
        }
    }

    async fn diff_since(&self, start: &Snapshot, max_bytes: usize) -> String {
        let now = self.snapshot().await;
        let mut sections = Vec::new();
        for path in start.changed_paths(&now) {
            sections.push(self.section(path, start.get(path), now.get(path)).await);
        }
        fit_sections(&sections, max_bytes)
    }

    async fn candidates(&self, text: &str, limit: usize) -> Vec<Candidate> {
        let terms = pick::terms(text);
        if terms.paths.is_empty() && terms.idents.is_empty() && terms.words.is_empty() {
            return Vec::new();
        }
        let args = [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ];
        let mut files = match git(&self.repo, &args).await {
            Ok(raw) => split_nul(&raw),
            Err(error) => {
                crate::logging::warn(&format!("jev-loop: git ls-files failed: {error:#}"));
                return Vec::new();
            }
        };
        files.sort();
        files.dedup();
        let searches = terms.idents.iter().map(|word| self.files_containing(word));
        let hits = futures::future::join_all(searches).await;
        let mut ranked = pick::rank(&files, &terms, &hits, limit);
        for candidate in &mut ranked {
            candidate.lines = matching_lines(&self.repo.join(&candidate.path), &candidate.words);
        }
        ranked
    }
}

/// Changed and new files at one moment, each with a size and modification
/// time fingerprint, so two snapshots show what changed in between.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    files: BTreeMap<String, FileState>,
}

#[derive(Clone, Debug, PartialEq)]
struct FileState {
    untracked: bool,
    /// None when the file is gone.
    fingerprint: Option<(u64, SystemTime)>,
}

impl Snapshot {
    fn from_status(repo: &Path, raw: &[u8]) -> Self {
        let files = parse_status(raw)
            .into_iter()
            .take(MAX_SNAPSHOT_FILES)
            .map(|(path, untracked)| {
                let fingerprint = fingerprint(&repo.join(&path));
                (
                    path,
                    FileState {
                        untracked,
                        fingerprint,
                    },
                )
            })
            .collect();
        Self { files }
    }

    fn get(&self, path: &str) -> Option<&FileState> {
        self.files.get(path)
    }

    /// Paths whose state differs between this snapshot and `later`.
    fn changed_paths<'a>(&'a self, later: &'a Snapshot) -> Vec<&'a str> {
        let paths: BTreeSet<&str> = self
            .files
            .keys()
            .chain(later.files.keys())
            .map(String::as_str)
            .collect();
        paths
            .into_iter()
            .filter(|path| self.files.get(*path) != later.files.get(*path))
            .collect()
    }
}

/// Paths in `git status --porcelain=v1 -z` output, each with whether it is
/// untracked. A rename or copy entry is followed by its source path, which is
/// skipped.
pub(crate) fn parse_status(raw: &[u8]) -> Vec<(String, bool)> {
    let mut entries = Vec::new();
    let mut fields = raw.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let (code, path) = (&field[..2], &field[3..]);
        if code.iter().any(|c| matches!(c, b'R' | b'C')) {
            fields.next();
        }
        entries.push((String::from_utf8_lossy(path).into_owned(), code == b"??"));
    }
    entries
}

fn fingerprint(path: &Path) -> Option<(u64, SystemTime)> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Some((
            metadata.len(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        )),
        _ => None,
    }
}

fn show_file(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_SHOWN_FILE_BYTES => {
            return format!("(file of {} bytes, not shown)", metadata.len());
        }
        Ok(_) => {}
        Err(error) => return format!("(unreadable: {error})"),
    }
    match std::fs::read(path) {
        Ok(bytes) if bytes.contains(&0) => format!("(binary file, {} bytes)", bytes.len()),
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("(unreadable: {error})"),
    }
}

/// Join diff sections in about `max_bytes`. When they do not all fit, each
/// file gets an equal share (its start is kept), and files past the budget
/// are listed by name.
pub(crate) fn fit_sections(sections: &[(String, String)], max_bytes: usize) -> String {
    let rendered: Vec<String> = sections
        .iter()
        .map(|(header, body)| format!("=== {header} ===\n{}\n", body.trim_end()))
        .collect();
    if rendered.iter().map(String::len).sum::<usize>() <= max_bytes {
        return rendered.concat();
    }
    let share = (max_bytes / sections.len().max(1))
        .max(MIN_SECTION_BYTES)
        .min(max_bytes);
    let mut text = String::new();
    let mut unshown = Vec::new();
    for (header, body) in sections {
        if !text.is_empty() && text.len() + share > max_bytes {
            unshown.push(header.as_str());
            continue;
        }
        let body = body.trim_end();
        let shown = cut(body, share.saturating_sub(header.len() + 48));
        text.push_str(&format!("=== {header} ===\n{shown}"));
        if shown.len() < body.len() {
            text.push_str(&format!(
                "\n... (cut: {} more bytes)",
                body.len() - shown.len()
            ));
        }
        text.push('\n');
    }
    if !unshown.is_empty() {
        let names = unshown.join(", ");
        text.push_str(&format!(
            "=== {} more changed files not shown: {} ===\n",
            unshown.len(),
            cut(&names, 1000)
        ));
    }
    text
}

/// The longest start of `text` within `max_bytes`, ending on a character
/// boundary.
pub(crate) fn cut(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Lines Jev sees for a candidate: those that mention the task's
/// identifiers, or the file's first lines when none do.
fn matching_lines(path: &Path, words: &[String]) -> Vec<String> {
    let text = match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() <= MAX_SHOWN_FILE_BYTES => {
            match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(_) => return Vec::new(),
            }
        }
        _ => return Vec::new(),
    };
    let shorten = |line: &str| -> String { line.trim().chars().take(160).collect() };
    let matching: Vec<String> = text
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            words.iter().any(|word| lower.contains(word.as_str()))
        })
        .take(LINES_PER_CANDIDATE)
        .map(shorten)
        .collect();
    if !matching.is_empty() {
        return matching;
    }
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .take(LINES_PER_CANDIDATE)
        .map(shorten)
        .collect()
}

fn split_nul(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Run git in the repository and return its standard output. `git grep`
/// exits 1 when nothing matches, which is not an error here.
async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["--no-optional-locks", "--literal-pathspecs"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| anyhow::anyhow!("could not run git: {error}"))?;
    let no_match = args.first() == Some(&"grep") && output.status.code() == Some(1);
    if no_match {
        return Ok(Vec::new());
    }
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "git {} failed: {}",
        args.first().copied().unwrap_or("command"),
        tail(stderr.trim(), 300)
    )
}

#[cfg(unix)]
async fn run_check_in(repo: &Path, command: &str, limit: Duration) -> LoopCheck {
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    if let Some(reason) = destructive_hold(command, repo) {
        return LoopCheck::not_run(format!(
            "jcode's destructive-command gate held it: {reason}"
        ));
    }
    let mut input = serde_json::json!({ "command": command });
    guard::hide_credentials(&mut input);
    let Some(isolated) = input["command"].as_str() else {
        return LoopCheck::not_run("could not prepare the command");
    };
    // A login shell, like the helper's own commands. `exec 2>&1` puts the
    // check's errors into the one captured stream, in order with its output.
    let mut shell = tokio::process::Command::new("bash");
    shell
        .arg("-lc")
        .arg(format!("exec 2>&1\n{isolated}"))
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(dir) = crate::tool::bash::tool_scratch_dir() {
        shell.env("TMPDIR", &dir).env("JCODE_SCRATCH_DIR", dir);
    }
    let started = std::time::Instant::now();
    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(error) => return LoopCheck::not_run(format!("could not start bash: {error}")),
    };
    let pid = child.id();
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(keep_tail(stdout, output.clone())));
    let status = tokio::time::timeout(limit, child.wait()).await;
    // Stop whatever the check left running, and everything on a timeout.
    if let Some(pid) = pid {
        kill_group(pid);
    }
    if status.is_err()
        && let Err(error) = child.wait().await
    {
        crate::logging::warn(&format!("jev-loop: check did not exit: {error}"));
    }
    if let Some(mut reader) = reader {
        match tokio::time::timeout(Duration::from_secs(2), &mut reader).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                crate::logging::warn(&format!("jev-loop: check output lost: {error}"));
            }
            Err(_) => {
                reader.abort();
                crate::logging::warn("jev-loop: check output still open after exit");
            }
        }
    }
    let bytes = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let text = String::from_utf8_lossy(&bytes);
    let output_tail = tail(text.trim_end(), CHECK_OUTPUT_TAIL_CHARS);
    let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    match status {
        Ok(Ok(status)) => {
            LoopCheck::finished(status.success(), status.code(), output_tail, seconds)
        }
        Ok(Err(error)) => LoopCheck::not_run(format!("could not wait for the check: {error}")),
        Err(_) => LoopCheck::timed_out(output_tail, seconds),
    }
}

#[cfg(not(unix))]
async fn run_check_in(_repo: &Path, _command: &str, _limit: Duration) -> LoopCheck {
    LoopCheck::unsupported()
}

/// Why jcode's destructive-command gate would hold `command`, if it would.
/// The bash tool lets a model justify a held command and try again; the loop
/// never does, so a held check is simply not run.
#[cfg(unix)]
pub(crate) fn destructive_hold(command: &str, repo: &Path) -> Option<String> {
    let context = jcode_command_risk::RiskContext {
        scratch_dir: crate::tool::bash::tool_scratch_dir(),
        ..jcode_command_risk::RiskContext::from_env(Some(repo.to_path_buf()))
    };
    let assessment = jcode_command_risk::assess(command, &context);
    if assessment.level.runs_immediately() {
        return None;
    }
    let reasons: Vec<&str> = assessment
        .findings
        .iter()
        .map(|finding| finding.reason.as_str())
        .collect();
    Some(if reasons.is_empty() {
        "it may delete or overwrite files".into()
    } else {
        reasons.join("; ")
    })
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    if let Err(error) = crate::platform::signal_detached_process_group(pid, libc::SIGKILL)
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        crate::logging::warn(&format!(
            "jev-loop: could not stop the check's processes: {error}"
        ));
    }
}

/// Read a check's output, keeping only the most recent bytes in `sink`.
#[cfg(unix)]
async fn keep_tail(
    mut stdout: tokio::process::ChildStdout,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    use tokio::io::AsyncReadExt;
    const KEEP_BYTES: usize = 64 * 1024;
    let mut chunk = vec![0u8; 8192];
    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut kept = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > KEEP_BYTES * 2 {
            let excess = kept.len() - KEEP_BYTES;
            kept.drain(..excess);
        }
    }
}
