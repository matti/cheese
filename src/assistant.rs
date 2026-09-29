//! Subscription-authenticated CLI runners; never fall back to API-key billing.
use serde::Deserialize;
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::PermissionsExt,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Default, Deserialize)]
pub struct Assistants {
    codex_home: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
}

impl Assistants {
    pub fn new() -> io::Result<Self> {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
        let Some(root) = root else {
            return Ok(Self::default());
        };
        match std::fs::read(root.join("battery/config.json")) {
            Ok(data) => serde_json::from_slice(&data).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    fn codex(&self) -> Command {
        let mut cmd = Command::new("codex");
        for key in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_ACCESS_TOKEN",
        ] {
            cmd.env_remove(key);
        }
        if std::env::var_os("CODEX_HOME").is_none()
            && let Some(root) = &self.codex_home
        {
            cmd.env("CODEX_HOME", root);
        }
        cmd
    }

    fn claude(&self) -> Command {
        // klaude selects the subscription's live credential store. A plain
        // claude auth check cannot see accounts managed by that wrapper.
        let mut cmd = Command::new(if on_path("klaude") {
            "klaude"
        } else {
            "claude"
        });
        cmd.env("KLAUDE_NO_AUTOSETUP", "1");
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_PROFILE",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_SIMPLE",
            "CLAUDECODE",
        ] {
            cmd.env_remove(key);
        }
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_none()
            && let Some(root) = &self.claude_config_dir
        {
            cmd.env("CLAUDE_CONFIG_DIR", root);
        }
        // Prevent settings from reintroducing API keys or apiKeyHelper. OAuth
        // credentials remain in the CLI's own credential store.
        cmd.args([
            "--setting-sources",
            "",
            "--settings",
            r#"{"forceLoginMethod":"claudeai","disableAllHooks":true}"#,
        ]);
        cmd
    }

    pub fn codex_ready(&self) -> bool {
        // Check before forcing the login method: Codex may log out mismatched
        // credentials when forced_login_method is applied at startup.
        let result = self
            .codex()
            .args(["login", "status"])
            .stdin(Stdio::null())
            .output();
        result.is_ok_and(|out| {
            out.status.success()
                && [&out.stdout, &out.stderr]
                    .iter()
                    .any(|s| String::from_utf8_lossy(s).contains("Logged in using ChatGPT"))
        })
    }

    pub fn require_claude(&self) -> io::Result<()> {
        let result = self
            .claude()
            .args(["auth", "status", "--json"])
            .stdin(Stdio::null())
            .output();
        if let Ok(out) = result
            && out.status.success()
            && let Ok(auth) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            && auth["loggedIn"] == true
            && matches!(
                auth["authMethod"].as_str(),
                Some("claude.ai" | "oauth_token")
            )
        {
            return Ok(());
        }
        Err(io::Error::other(
            "No working subscription login. Check `codex login status` or `klaude auth status` (plain `claude auth status` if klaude is not installed). Custom profiles can be set in ~/.config/battery/config.json.",
        ))
    }

    pub fn analyze(
        &self,
        prompt: &Path,
        codex_ready: bool,
        ui: &mut crate::wtf_ui::Progress,
    ) -> io::Result<(String, &'static str)> {
        if codex_ready {
            ui.status(3, "Codex is analyzing the measurements")?;
            let result = run(
                self.codex().args([
                    "exec",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--sandbox",
                    "read-only",
                    "-c",
                    "approval_policy=\"never\"",
                    "-c",
                    "model_provider=\"openai\"",
                    "-c",
                    "forced_login_method=\"chatgpt\"",
                    "-",
                ]),
                prompt,
                "codex",
                ui,
            );
            match result {
                Ok(answer) => return Ok((answer, "Codex")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                Err(_) => {
                    ui.status(3, "Codex failed · switching to Claude")?;
                }
            }
            self.require_claude()?;
        }
        ui.status(3, "Claude is analyzing the measurements")?;
        run(
            self.claude().args([
                "--print",
                "--no-session-persistence",
                "--output-format",
                "text",
                "--permission-mode",
                "dontAsk",
                "--tools",
                "",
                "--strict-mcp-config",
            ]),
            prompt,
            "claude",
            ui,
        )
        .map(|answer| (answer, "Claude"))
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            std::fs::metadata(dir.join(program))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

fn run(
    command: &mut Command,
    prompt: &Path,
    name: &str,
    ui: &mut crate::wtf_ui::Progress,
) -> io::Result<String> {
    let output_path = prompt.with_extension(format!("{name}.txt"));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)?;
    let log_path = prompt.with_extension(format!("{name}.log"));
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_path)?;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::from(File::open(prompt)?))
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(log))
        .spawn()?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if let Err(e) = ui.wait(Duration::from_millis(80)) {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(e);
        }
    };
    if matches!(status.signal(), Some(libc::SIGINT | libc::SIGTERM))
        || matches!(status.code(), Some(130 | 143))
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("{name} interrupted"),
        ));
    }
    let answer = std::fs::read_to_string(output_path)?;
    if !status.success() || answer.trim().is_empty() {
        return Err(io::Error::other(format!(
            "{name}: analysis failed ({status}). Log: {}",
            log_path.display()
        )));
    }
    Ok(answer)
}
