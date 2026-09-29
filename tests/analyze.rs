use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "battery wtf test {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn mock(&self, name: &str) {
        let script = if name == "codex" {
            r#"#!/bin/sh
if [ -n "${CODEX_API_KEY+x}${OPENAI_API_KEY+x}${OPENAI_BASE_URL+x}" ]; then exit 99; fi
if [ "$1" = login ]; then
    echo "Logged in using ${CODEX_AUTH:-ChatGPT}" >&2
    exit 0
fi
printf '%s\n' "$@" > codex.args
printf '%s' "$CODEX_HOME" > codex.home
/bin/cat > codex.stdin
echo 'hook: diagnostic-noise' >&2
echo 'Codex analysis'
if [ "${CODEX_INTERRUPT:-}" = yes ]; then exit 130; fi
exit "${CODEX_EXIT:-0}"
"#
        } else {
            r#"#!/bin/sh
if [ -n "${ANTHROPIC_API_KEY+x}${ANTHROPIC_AUTH_TOKEN+x}${ANTHROPIC_BASE_URL+x}${CLAUDE_CODE_USE_BEDROCK+x}" ]; then exit 99; fi
case "$*" in
    *"auth status --json"*)
        printf '{"loggedIn":true,"authMethod":"%s"}\n' "${CLAUDE_AUTH:-claude.ai}"
        exit 0;;
esac
printf '%s\n' "$@" > claude.args
/bin/cat > claude.stdin
echo 'hook: diagnostic-noise' >&2
echo 'Claude analysis'
exit "${CLAUDE_EXIT:-0}"
"#
        };
        let path = self.0.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_battery"));
        command
            .current_dir(&self.0)
            .env("PATH", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .args(["--dir", "evidence with spaces", "wtf"]);
        for key in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "OPENAI_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_USE_BEDROCK",
        ] {
            command.env(key, "test-override");
        }
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn missing_clis_fail_before_collecting() {
    let f = Fixture::new();
    let out = f.command().output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No working subscription login"));
    assert!(!f.0.join("evidence with spaces").exists());
}

#[test]
fn codex_uses_subscription_profile_and_five_second_evidence() {
    let f = Fixture::new();
    f.mock("codex");
    f.mock("claude");
    fs::create_dir(f.0.join("battery")).unwrap();
    fs::write(
        f.0.join("battery/config.json"),
        r#"{"codex_home":"/test/profile with spaces"}"#,
    )
    .unwrap();
    let out = f.command().output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Codex analysis\n");
    assert_eq!(
        fs::read_to_string(f.0.join("codex.home")).unwrap(),
        "/test/profile with spaces"
    );
    assert!(!f.0.join("claude.stdin").exists());
    let args = fs::read_to_string(f.0.join("codex.args")).unwrap();
    for flag in [
        "exec\n",
        "--skip-git-repo-check\n",
        "--sandbox\nread-only\n",
        "approval_policy=\"never\"\n",
        "forced_login_method=\"chatgpt\"\n",
        "model_provider=\"openai\"\n",
    ] {
        assert!(args.contains(flag), "missing {flag}");
    }
    let prompt = fs::read_to_string(f.0.join("codex.stdin")).unwrap();
    let (_, data) = prompt.split_once("MEASUREMENTS (JSON):\n").unwrap();
    let evidence: serde_json::Value = serde_json::from_str(data).unwrap();
    assert!(!evidence["samples"].as_array().unwrap().is_empty());
    let inspections = evidence["inspections"].as_array().unwrap();
    assert!(inspections.len() <= 3);
    assert!(inspections.iter().all(|i| i.get("vm").is_some()
        || i.get("stack").is_some()
        || i.get("unavailable").is_some()));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Measuring heat and load · 5 s"));
    assert!(
        evidence["top_intervals"]["output"]
            .as_str()
            .unwrap()
            .contains("CPU usage:")
    );
    assert!(
        evidence["samples"][0]["listed_process_count"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn failed_codex_reuses_exact_prompt_with_claude() {
    let f = Fixture::new();
    f.mock("codex");
    f.mock("claude");
    let out = f
        .command()
        .env("CODEX_EXIT", "17")
        .args(["--seconds", "2"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Claude analysis\n");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("diagnostic-noise"));
    let logs: Vec<_> = fs::read_dir(f.0.join("evidence with spaces"))
        .unwrap()
        .map(|p| p.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect();
    assert_eq!(logs.len(), 2);
    assert!(
        logs.iter()
            .all(|p| fs::read_to_string(p).unwrap().contains("diagnostic-noise"))
    );
    assert_eq!(
        fs::read(f.0.join("codex.stdin")).unwrap(),
        fs::read(f.0.join("claude.stdin")).unwrap()
    );
    let args = fs::read_to_string(f.0.join("claude.args")).unwrap();
    assert!(args.contains("--print\n"));
    assert!(args.contains("--tools\n\n"));
    assert!(args.contains("\"forceLoginMethod\":\"claudeai\""));
}

#[test]
fn missing_codex_uses_claude() {
    let f = Fixture::new();
    f.mock("claude");
    let out = f.command().args(["--seconds", "2"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Claude analysis\n");
}

#[test]
fn api_login_is_skipped_without_running_or_logging_out_codex() {
    let f = Fixture::new();
    f.mock("codex");
    f.mock("claude");
    let out = f
        .command()
        .env("CODEX_AUTH", "API key")
        .args(["--seconds", "2"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!f.0.join("codex.stdin").exists());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Claude analysis\n");
}

#[test]
fn claude_api_login_is_rejected_before_collecting() {
    let f = Fixture::new();
    f.mock("claude");
    let out = f.command().env("CLAUDE_AUTH", "api_key").output().unwrap();
    assert!(!out.status.success());
    assert!(!f.0.join("evidence with spaces").exists());
    assert!(!f.0.join("claude.stdin").exists());
}

#[test]
fn both_fail_returns_failure_and_no_fake_answer() {
    let f = Fixture::new();
    f.mock("codex");
    f.mock("claude");
    let out = f
        .command()
        .env("CODEX_EXIT", "17")
        .env("CLAUDE_EXIT", "19")
        .args(["--seconds", "2"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn interrupt_does_not_start_claude() {
    let f = Fixture::new();
    f.mock("codex");
    f.mock("claude");
    let out = f
        .command()
        .env("CODEX_INTERRUPT", "yes")
        .args(["--seconds", "2"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!f.0.join("claude.stdin").exists());
}

#[test]
fn klaude_is_used_for_both_auth_and_analysis_when_plain_claude_is_logged_out() {
    let f = Fixture::new();
    f.mock("codex");
    f.mock("claude");
    let wrapper = f.0.join("klaude");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
if [ "$KLAUDE_NO_AUTOSETUP" != 1 ]; then exit 98; fi
if [ -t 0 ]; then exit 97; fi
printf 'called\n' >> klaude.calls
export CLAUDE_AUTH=claude.ai
exec claude "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let out = f
        .command()
        .env("CODEX_EXIT", "17")
        .env("CLAUDE_AUTH", "none")
        .args(["--seconds", "2"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Claude analysis\n");
    assert_eq!(
        fs::read_to_string(f.0.join("klaude.calls")).unwrap(),
        "called\ncalled\n"
    );
    assert_eq!(
        fs::read(f.0.join("codex.stdin")).unwrap(),
        fs::read(f.0.join("claude.stdin")).unwrap()
    );
}
