//! Tests for the loop's code-level guard: blocked commands, the tool-call
//! and spending caps, and running shell commands without cloud or GitHub
//! logins.

use super::config::BLOCKED_COMMANDS;
use super::guard::blocked_command;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[test]
fn blocked_commands_are_caught_through_chains_wrappers_and_nesting() {
    let blocked = |command: &str| blocked_command(command, BLOCKED_COMMANDS);
    for (command, expected) in [
        ("git push", "git push"),
        ("git push origin main --force", "git push"),
        ("cargo test && git push", "git push"),
        ("git -C /repo push", "git push"),
        ("git reset --hard HEAD~1", "git reset --hard"),
        ("git clean -fdx", "git clean"),
        ("rm -rf target", "rm -rf"),
        ("rm -r -f target", "rm -rf"),
        ("rm -Rf target", "rm -rf"),
        ("rm --recursive --force target", "rm -rf"),
        ("/bin/rm -fr target", "rm -rf"),
        ("sudo apt-get install x", "sudo"),
        ("FOO=1 env -u HOME nice -n 5 git push", "git push"),
        ("timeout 10 git push", "git push"),
        ("bash -c 'git push'", "git push"),
        ("sh -lc \"cd x && rm -rf y\"", "rm -rf"),
        ("eval git push", "git push"),
        ("echo ok; sudo reboot", "sudo"),
        ("echo $(git push)", "git push"),
        ("x=`git push origin`", "git push"),
        ("echo \"$(cd x && rm -rf y)\"", "rm -rf"),
    ] {
        assert_eq!(blocked(command).as_deref(), Some(expected), "{command}");
    }
    for command in [
        "git status",
        "git reset --soft HEAD~1",
        "git log --grep push",
        "rm target/file.txt",
        "rm -r target",
        "cargo test -- --nocapture",
        "echo 'git push'",
        "grep -rn sudo src",
        "echo $(git status)",
        "echo 'no $(closing paren'",
    ] {
        assert_eq!(blocked(command), None, "{command}");
    }
}

#[test]
fn commands_that_change_state_outside_the_repo_are_blocked() {
    let blocked = |command: &str| blocked_command(command, BLOCKED_COMMANDS);
    for (command, expected) in [
        // GitHub, cloud, and cluster CLIs, and remote shells.
        ("gh workflow run deploy-prod.yml", "gh"),
        ("gh pr create --fill", "gh"),
        ("/usr/bin/gh api repos/o/r", "gh"),
        (
            "aws ecs update-service --cluster prod --service api --force-new-deployment",
            "aws",
        ),
        ("AWS_PROFILE=prod aws s3 ls", "aws"),
        ("gcloud run deploy api", "gcloud"),
        ("az webapp up", "az"),
        ("kubectl --context prod apply -f k8s/", "kubectl"),
        ("ssh deploy@host 'systemctl restart api'", "ssh"),
        ("scp build.tar deploy@host:/srv", "scp"),
        // Infrastructure changes, with options before the subcommand.
        (
            "terraform -chdir=infra/terraform apply -auto-approve",
            "terraform apply",
        ),
        ("terraform destroy", "terraform destroy"),
        ("terraform state rm aws_s3_bucket.logs", "terraform state"),
        ("tofu apply", "tofu apply"),
        ("pulumi up --yes", "pulumi up"),
        ("cdk deploy --all", "cdk deploy"),
        ("helm -n prod upgrade api ./chart", "helm upgrade"),
        (
            "helm --kube-context prod install api ./chart",
            "helm install",
        ),
        // Hosting deploys, directly or through a package runner.
        ("vercel --prod", "vercel"),
        ("npx vercel deploy --prod", "vercel"),
        ("npx vercel@latest --prod", "vercel"),
        ("fly deploy", "fly"),
        ("npx wrangler deploy", "wrangler"),
        ("npx netlify-cli deploy --prod", "netlify-cli"),
        ("npx firebase-tools deploy", "firebase-tools deploy"),
        ("firebase --project prod deploy", "firebase deploy"),
        ("npx aws-cdk deploy", "aws-cdk deploy"),
        ("sam deploy --guided", "sam deploy"),
        ("npx serverless deploy", "serverless deploy"),
        // Image pushes.
        ("docker push registry/app:1", "docker push"),
        ("docker --context prod push registry/app:1", "docker push"),
        ("docker image push registry/app:1", "docker image push"),
        (
            "docker buildx build --push -t registry/app .",
            "docker buildx build --push",
        ),
        ("podman push registry/app:1", "podman push"),
        // Package releases, including through runners and toolchains.
        ("npm publish", "npm publish"),
        ("npm --workspace api publish", "npm publish"),
        ("npm --tag beta publish", "npm publish"),
        (
            "docker --tlscacert ca.pem push registry/app:1",
            "docker push",
        ),
        ("pnpm -r publish", "pnpm publish"),
        ("cargo +nightly publish", "cargo publish"),
        ("uv publish", "uv publish"),
        ("twine upload dist/*", "twine upload"),
        ("python -m twine upload dist/*", "twine upload"),
        ("python3 -m twine upload dist/*", "twine upload"),
        ("uv run twine upload dist/*", "twine upload"),
        ("uvx twine upload dist/*", "twine upload"),
        ("poetry publish --build", "poetry publish"),
        // Runners, pipes, and nesting reach the program they start.
        ("uv run aws s3 ls", "aws"),
        ("uv run --with awscli -- aws s3 ls", "aws"),
        ("uv run --directory apps/ops-cli aws s3 ls", "aws"),
        ("poetry run aws s3 ls", "aws"),
        ("npm exec -- vercel", "vercel"),
        ("pnpm exec vercel", "vercel"),
        ("pnpm dlx vercel", "vercel"),
        ("bunx vercel", "vercel"),
        ("npx -p @aws-cdk/cli cdk deploy", "cdk deploy"),
        ("python -m awscli s3 ls", "awscli"),
        ("uvx --from awscli aws s3 ls", "aws"),
        ("echo x | xargs -n 1 aws s3 rm", "aws"),
        (
            "find dist -name '*.whl' -exec twine upload {} +",
            "twine upload",
        ),
        ("npx npx vercel", "vercel"),
        ("uv run bash -c 'terraform apply'", "terraform apply"),
        ("bash -c 'uv run aws s3 ls'", "aws"),
        ("cargo test && gh release create v1", "gh"),
        ("timeout 60 kubectl apply -f x.yaml", "kubectl"),
        // Other remote state changes.
        ("git lfs push origin main", "git lfs push"),
        ("terraform refresh", "terraform refresh"),
        (
            "terraform init -migrate-state",
            "terraform init -migrate-state",
        ),
        (
            "docker build --push -t registry/app .",
            "docker build --push",
        ),
        ("npm unpublish pkg@1.0.0", "npm unpublish"),
        ("cargo yank --version 1.0.0", "cargo yank"),
    ] {
        assert_eq!(blocked(command).as_deref(), Some(expected), "{command}");
    }
    // Local checks and read-only commands stay allowed.
    for command in [
        "terraform plan",
        "terraform -chdir=infra/terraform validate",
        "terraform fmt -check",
        "terraform init -backend=false",
        "terraform init",
        "tofu plan",
        "helm lint ./chart",
        "helm template api ./chart",
        "cdk synth",
        "npx aws-cdk synth",
        "sam build",
        "sam local invoke Fn",
        "firebase emulators:exec 'npm test'",
        "npx firebase-tools emulators:exec 'npm test'",
        "docker build -t app .",
        "git lfs pull",
        "docker compose up -d",
        "docker compose -f docker-compose.yml run --rm api pytest",
        "docker run --rm app pytest",
        "docker buildx build -t app .",
        "npm test",
        "npm run build",
        "npm --workspace api test",
        "npx vitest run",
        "npx vitest@2 run",
        "npx tsc --noEmit",
        "npm install @scope/pkg@1.2.3",
        "pnpm -r test",
        "pnpm exec vitest",
        "yarn test",
        "cargo test publish",
        "cargo +nightly test",
        "cargo run --bin publish-report",
        "uv run pytest -q",
        "uv run --directory apps/api pytest tests/test_publish.py",
        "uvx ruff check .",
        "uv build",
        "python -m pytest -k publish",
        "python -m build",
        "python -c 'print(1)'",
        "python scripts/check.py",
        "poetry run pytest",
        "twine check dist/*",
        "grep -rn 'aws ' src",
        "echo 'gh workflow run x'",
        "ls ~/.aws",
        "cat infra/terraform/main.tf",
        "git log --oneline -5",
        "git -C apps/web status",
        "find . -name '*.py' -exec grep -l publish {} +",
        "echo a b | xargs -n 1 echo",
        "make test",
        "rg ssh docs",
    ] {
        assert_eq!(blocked(command), None, "{command}");
    }
}

/// Records every call that reaches the real tool.
struct CountingTool(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl crate::tool::Tool for CountingTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "counts calls"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"command": {"type": "string"}}})
    }
    async fn execute(
        &self,
        input: Value,
        _ctx: crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        self.0
            .lock()
            .unwrap()
            .push(input["command"].as_str().unwrap_or("").into());
        Ok(crate::tool::ToolOutput::new("ran"))
    }
}

fn tool_ctx() -> crate::tool::ToolContext {
    crate::tool::ToolContext {
        session_id: "jev-loop-test".into(),
        message_id: "m".into(),
        tool_call_id: "t".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    }
}

#[tokio::test]
async fn guarded_tool_blocks_commands_caps_calls_and_cancels_the_turn() {
    use super::guard::{GuardedTool, SessionGuard};
    use crate::tool::Tool;

    let ran = Arc::new(Mutex::new(Vec::new()));
    let guard = SessionGuard::new(3, None, BLOCKED_COMMANDS);
    let cancel = crate::agent::InterruptSignal::new();
    guard.attach_cancel(cancel.clone());
    let tool = GuardedTool::new(Arc::new(CountingTool(ran.clone())), guard.clone());
    assert_eq!(tool.name(), "bash");
    assert_eq!(tool.to_definition().name, "bash");

    // A blocked command never reaches the real tool, and the turn goes on.
    let refusal = tool
        .execute(json!({"command": "cargo test && git push"}), tool_ctx())
        .await
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("`git push` is never allowed"), "{refusal}");
    assert!(ran.lock().unwrap().is_empty());
    assert!(!cancel.is_set());

    // Allowed commands run until the cap; the call over the cap stops the turn.
    for _ in 0..2 {
        tool.execute(json!({"command": "cargo test"}), tool_ctx())
            .await
            .unwrap();
    }
    assert_eq!(ran.lock().unwrap().len(), 2);
    let over = tool
        .execute(json!({"command": "cargo test"}), tool_ctx())
        .await
        .unwrap_err()
        .to_string();
    assert!(over.contains("tool-call limit reached (3 calls)"), "{over}");
    assert!(
        cancel.is_set(),
        "hitting the cap must cancel the running turn"
    );
    assert_eq!(
        guard.stop_reason().as_deref(),
        Some("tool-call limit reached (3 calls)")
    );
    // Once stopped, nothing else runs.
    assert!(
        tool.execute(json!({"command": "ls"}), tool_ctx())
            .await
            .is_err()
    );
    assert_eq!(ran.lock().unwrap().len(), 2);
}

/// Loop shell commands run without this machine's cloud and GitHub logins,
/// while values a command sets itself still apply.
#[cfg(unix)]
#[tokio::test]
async fn loop_shell_commands_run_without_cloud_or_github_logins() {
    use super::guard::{GuardedTool, SessionGuard, hide_credentials};
    use crate::tool::Tool;

    // The wrapper hands the real tool the isolated command.
    let ran = Arc::new(Mutex::new(Vec::new()));
    let guard = SessionGuard::new(10, None, BLOCKED_COMMANDS);
    let tool = GuardedTool::new(Arc::new(CountingTool(ran.clone())), guard);
    tool.execute(json!({"command": "pytest -q"}), tool_ctx())
        .await
        .unwrap();
    let sent = ran.lock().unwrap()[0].clone();
    assert!(sent.contains("AWS_CONFIG_FILE=/dev/null"), "{sent}");
    assert!(sent.ends_with("\npytest -q"), "{sent}");

    // And the lines do what they say in a real shell.
    let mut input = json!({"command": "AWS_ACCESS_KEY_ID=test printenv AWS_ACCESS_KEY_ID; \
        printf '%s|%s|%s|%s|%s\\n' \"${AWS_PROFILE-unset}\" \"${GH_TOKEN-unset}\" \
        \"$AWS_CONFIG_FILE\" \"$GH_CONFIG_DIR\" \"$KUBECONFIG\""});
    hide_credentials(&mut input);
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(input["command"].as_str().unwrap())
        .env("AWS_PROFILE", "example-production")
        .env("GH_TOKEN", "secret")
        .output()
        .expect("bash runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        "test\nunset|unset|/dev/null|/dev/null/gh|/dev/null\n",
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Git hands a loop command no password over HTTPS. Without the isolation
/// lines, a `store` credential helper (plain, URL-scoped, or passed down by a
/// parent's `git -c`), an inherited `GIT_ASKPASS` or `SSH_ASKPASS` program,
/// and `core.askPass` each give one out; with them, none does. A
/// `GIT_CONFIG_COUNT` entry the parent set keeps working, and a command's own
/// `GIT_ASKPASS` still applies.
#[cfg(unix)]
#[test]
fn git_hands_loop_commands_no_password() {
    use super::guard::hide_credentials;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    std::fs::write(
        path("store"),
        "https://someone:stored-secret@git.example.invalid\n",
    )
    .expect("write credential store");
    let askpass = path("askpass.sh");
    std::fs::write(&askpass, "#!/bin/sh\necho askpass-secret\n").expect("write askpass");
    std::process::Command::new("chmod")
        .args(["+x", &askpass])
        .status()
        .expect("chmod askpass");
    let store_helper = format!("store --file={}", path("store"));
    let write_config = |name: &str, body: String| {
        std::fs::write(path(name), body).expect("write git config");
        path(name)
    };
    let plain = write_config(
        "plain",
        format!("[credential]\n\thelper = {store_helper}\n"),
    );
    let scoped = write_config(
        "scoped",
        format!("[credential \"https://git.example.invalid\"]\n\thelper = {store_helper}\n"),
    );
    let core = write_config("core", format!("[core]\n\taskPass = {askpass}\n"));

    // Prints the password git would send for the host, or nothing.
    let fill = "printf 'protocol=https\\nhost=git.example.invalid\\n\\n' \
        | git credential fill 2>/dev/null | sed -n 's/^password=//p'";
    let run = |command: &str, isolate: bool, global: &str, extra: &[(&str, &str)]| {
        let mut input = json!({ "command": command });
        if isolate {
            hide_credentials(&mut input);
        }
        let mut shell = std::process::Command::new("bash");
        shell
            .arg("-c")
            .arg(input["command"].as_str().unwrap())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", global)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS");
        for (key, value) in extra {
            shell.env(key, value);
        }
        let output = shell.output().expect("bash runs");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    // What a parent's `git -c credential.helper=...` exports to children.
    let passed_down = format!("'credential.helper'='{store_helper}'");

    for (case, global, extra, secret) in [
        ("store helper", plain.as_str(), &[][..], "stored-secret"),
        (
            "URL-scoped store helper",
            scoped.as_str(),
            &[][..],
            "stored-secret",
        ),
        (
            "helper passed down by git -c",
            "/dev/null",
            &[("GIT_CONFIG_PARAMETERS", passed_down.as_str())][..],
            "stored-secret",
        ),
        ("core.askPass", core.as_str(), &[][..], "askpass-secret"),
        (
            "inherited GIT_ASKPASS",
            "/dev/null",
            &[("GIT_ASKPASS", askpass.as_str())][..],
            "askpass-secret",
        ),
        (
            "inherited SSH_ASKPASS",
            "/dev/null",
            &[("SSH_ASKPASS", askpass.as_str())][..],
            "askpass-secret",
        ),
    ] {
        assert_eq!(
            run(fill, false, global, extra),
            secret,
            "{case}, no isolation"
        );
        assert_eq!(run(fill, true, global, extra), "", "{case}, isolated");
    }

    // The parent's own GIT_CONFIG_COUNT entry survives the added ones.
    let marker = [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "jevtest.marker"),
        ("GIT_CONFIG_VALUE_0", "kept"),
    ];
    assert_eq!(
        run(
            "git config --get jevtest.marker",
            true,
            "/dev/null",
            &marker
        ),
        "kept"
    );
    // A command that sets its own askpass still gets it. `export` because a
    // plain `VAR=x` prefix would reach only `printf`, the pipeline's first
    // command.
    assert_eq!(
        run(
            &format!("export GIT_ASKPASS={askpass}; {fill}"),
            true,
            "/dev/null",
            &[]
        ),
        "askpass-secret"
    );
}

/// End to end through the guard, as a loop helper's shell command runs: a
/// script sees none of the machine's AWS settings, even when the parent
/// process points at a real profile file. The inner tool runs the command
/// with `bash -c` in the working directory, as jcode's bash tool does.
#[cfg(unix)]
#[tokio::test]
async fn guarded_shell_scripts_see_no_cloud_logins_from_the_parent() {
    use super::guard::{GuardedTool, SessionGuard};
    use crate::tool::Tool;

    struct ShellTool;

    #[async_trait]
    impl crate::tool::Tool for ShellTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "runs bash -c"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {"command": {"type": "string"}}})
        }
        async fn execute(
            &self,
            input: Value,
            ctx: crate::tool::ToolContext,
        ) -> Result<crate::tool::ToolOutput> {
            let mut command = std::process::Command::new("bash");
            command
                .arg("-c")
                .arg(input["command"].as_str().unwrap_or(""));
            if let Some(dir) = ctx.working_dir {
                command.current_dir(dir);
            }
            let output = command.output()?;
            Ok(crate::tool::ToolOutput::new(
                String::from_utf8_lossy(&output.stdout).to_string(),
            ))
        }
    }

    let home = tempfile::tempdir().expect("tempdir");
    let config = home.path().join("aws_config");
    std::fs::write(
        &config,
        "[profile example-production]\nregion = eu-central-1\n",
    )
    .expect("write fake aws config");
    std::fs::write(
        home.path().join("report.sh"),
        "printf '%s|%s|%s\\n' \"$AWS_CONFIG_FILE\" \"${AWS_PROFILE-unset}\" \
         \"$(cat \"$AWS_CONFIG_FILE\" 2>/dev/null | grep -c profile)\"\n",
    )
    .expect("write script");
    let guard = SessionGuard::new(5, None, BLOCKED_COMMANDS);
    let tool = GuardedTool::new(Arc::new(ShellTool), guard);
    let mut ctx = tool_ctx();
    ctx.working_dir = Some(home.path().to_path_buf());

    // The command the helper sends first points at the machine's login, the
    // way a developer shell would; the guard's lines must still win for the
    // script it starts, while the command's own later assignments apply.
    let command = format!(
        "export AWS_CONFIG_FILE={} AWS_PROFILE=example-production; bash report.sh",
        config.display()
    );
    let leaked = tool
        .execute(json!({ "command": command }), ctx.clone())
        .await
        .expect("script runs");
    assert_eq!(
        leaked.output.trim(),
        format!("{}|example-production|1", config.display()),
        "a command's own exports must still apply"
    );

    let isolated = tool
        .execute(json!({"command": "bash report.sh"}), ctx)
        .await
        .expect("script runs");
    assert_eq!(isolated.output.trim(), "/dev/null|unset|0");

    // The added lines must not trip jcode's destructive-command gate, or
    // every loop shell command would be held for a justification.
    let mut input = json!({"command": "pytest -q"});
    super::guard::hide_credentials(&mut input);
    let assessment = jcode_command_risk::assess(
        input["command"].as_str().unwrap(),
        &jcode_command_risk::RiskContext::from_env(Some(home.path().to_path_buf())),
    );
    assert!(
        assessment.level.runs_immediately(),
        "{:?}",
        assessment.findings
    );
}

/// The same check through jcode's real `bash` tool, taken from the registry
/// exactly as a loop helper's session gets it.
#[cfg(unix)]
#[tokio::test]
async fn real_bash_tool_in_a_loop_session_sees_no_cloud_logins() {
    use super::guard::{GuardedTool, SessionGuard};
    use crate::tool::Tool;

    struct NoModel;

    #[async_trait]
    impl crate::provider::Provider for NoModel {
        async fn complete(
            &self,
            _messages: &[crate::message::Message],
            _tools: &[crate::message::ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<crate::provider::EventStream> {
            anyhow::bail!("no model in this test")
        }
        fn name(&self) -> &str {
            "test"
        }
        fn fork(&self) -> Arc<dyn crate::provider::Provider> {
            Arc::new(NoModel)
        }
    }

    let repo = tempfile::tempdir().expect("tempdir");
    let config = repo.path().join("aws_config");
    std::fs::write(
        &config,
        "[profile example-production]\nregion = eu-central-1\n",
    )
    .expect("write fake aws config");
    let registry = crate::tool::Registry::new(Arc::new(NoModel)).await;
    let bash = registry.unregister("bash").await.expect("bash tool exists");
    let tool = GuardedTool::new(bash, SessionGuard::new(5, None, BLOCKED_COMMANDS));
    let mut ctx = tool_ctx();
    ctx.working_dir = Some(repo.path().to_path_buf());

    // A child process started by the command reads the environment the way a
    // deploy script or SDK would.
    let output = tool
        .execute(
            json!({"command": "bash -c 'printf \"%s|%s\\n\" \"$AWS_CONFIG_FILE\" \"${GH_TOKEN-unset}\"'"}),
            ctx.clone(),
        )
        .await
        .expect("command runs");
    assert!(
        output.output.contains("/dev/null|unset"),
        "{}",
        output.output
    );
    assert!(!output.output.contains(config.to_str().unwrap()));

    // The bash tool's own checks still see the helper's command, not the
    // isolation lines in front of it: an in-place `sed` edit still gets the
    // "use the edit tool" note.
    std::fs::write(repo.path().join("f.txt"), "a\n").expect("write file");
    let edited = tool
        .execute(json!({"command": "sed -i 's/a/b/' f.txt"}), ctx)
        .await
        .expect("sed runs");
    assert!(
        edited.output.contains("edits files in place"),
        "{}",
        edited.output
    );
    assert_eq!(
        edited.output.matches("edits files in place").count(),
        1,
        "the hint appears once even when the bash tool also finds it"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
        "b\n"
    );
}

#[tokio::test]
async fn spending_over_budget_stops_the_session() {
    use super::guard::{GuardedTool, SessionGuard};
    use crate::tool::Tool;

    let ran = Arc::new(Mutex::new(Vec::new()));
    let guard = SessionGuard::new(40, Some(2.0), BLOCKED_COMMANDS);
    let cancel = crate::agent::InterruptSignal::new();
    guard.attach_cancel(cancel.clone());
    let tool = GuardedTool::new(Arc::new(CountingTool(ran.clone())), guard.clone());

    guard.record_spend(1.99);
    assert!(guard.stop_reason().is_none() && !cancel.is_set());
    tool.execute(json!({"command": "make"}), tool_ctx())
        .await
        .unwrap();

    guard.record_spend(2.01);
    assert!(cancel.is_set());
    assert_eq!(
        guard.stop_reason().as_deref(),
        Some("spending cap reached ($2.01 of $2.00 for this attempt)")
    );
    assert!(
        tool.execute(json!({"command": "make"}), tool_ctx())
            .await
            .is_err()
    );
    assert_eq!(ran.lock().unwrap().len(), 1);
    assert_eq!(
        guard.tool_calls(),
        1,
        "refused calls after a stop are not counted"
    );

    // Sub-cent budgets stay readable instead of printing `$0.00 of $0.00`.
    let tiny = SessionGuard::new(40, Some(0.001), BLOCKED_COMMANDS);
    tiny.record_spend(0.0042);
    assert_eq!(
        tiny.stop_reason().as_deref(),
        Some("spending cap reached ($0.0042 of $0.0010 for this attempt)")
    );
}
