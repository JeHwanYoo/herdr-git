use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

mod graphics;
mod sidebar;
mod update;

pub use graphics::{GraphicsPlacement, GraphicsSurface};
pub(crate) use update::{install_update, latest_version, save_skipped_version, skipped_version};

const PLUGIN_ID: &str = "io.github.jehwanyoo.herdr-git";
const PANE_TITLE: &str = "Git";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HerdrAgent {
    pub pane_id: String,
    pub label: String,
    pub cwd: String,
    pub workspace: String,
    pub agent_type: String,
    pub status: String,
    pub session: Option<String>,
}

pub fn toggle_git_pane() -> ExitCode {
    let context = env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let target_pane = env::var("HERDR_PANE_ID").ok().or_else(|| {
        context
            .as_ref()
            .and_then(|value| find_string(value, &["pane_id", "focused_pane_id"]))
    });
    let cwd = context.as_ref().and_then(|value| {
        find_string(
            value,
            &[
                "focused_pane_cwd",
                "foreground_cwd",
                "worktree_path",
                "workspace_cwd",
                "cwd",
            ],
        )
    });

    if let Some(tab) = env::var("HERDR_TAB_ID").ok().or_else(|| {
        context
            .as_ref()
            .and_then(|value| find_string(value, &["tab_id"]))
    }) && let Err(error) = sidebar::recover(&tab)
    {
        eprintln!("failed to restore Git pane layout: {error}");
        return ExitCode::FAILURE;
    }

    if let Some(target_pane) = target_pane.as_deref() {
        let output = match herdr_command().args(["pane", "list"]).output() {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                eprintln!(
                    "failed to list Herdr panes: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("failed to list Herdr panes: {error}");
                return ExitCode::FAILURE;
            }
        };
        let panes: Value = match serde_json::from_slice(&output.stdout) {
            Ok(panes) => panes,
            Err(error) => {
                eprintln!("Herdr returned invalid pane JSON: {error}");
                return ExitCode::FAILURE;
            }
        };
        let pane_ids = git_panes_in_tab(&panes, target_pane);
        if !pane_ids.is_empty() {
            let mut all_closed = true;
            for pane_id in pane_ids {
                let closed = herdr_command()
                    .args(["plugin", "pane", "close", &pane_id])
                    .status()
                    .is_ok_and(|status| status.success());
                all_closed &= closed;
            }
            return if all_closed {
                ExitCode::SUCCESS
            } else {
                eprintln!("failed to close every Git pane in the current tab");
                ExitCode::FAILURE
            };
        }
    }

    match sidebar::open(target_pane.as_deref(), cwd.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("failed to open Git pane: {error}");
            ExitCode::FAILURE
        }
    }
}

pub fn agent_request(root: &Path, prompt: &str, amend: bool) -> String {
    let intent = if amend {
        "Amend the current HEAD commit in the repository below."
    } else {
        "Commit the staged changes in the repository below."
    };
    let mut request = format!("{intent}\nRepository: {}", root.display());
    if !prompt.trim().is_empty() {
        request.push_str("\n\nAdditional request:\n");
        request.push_str(prompt.trim());
    }
    request
}

pub fn list_agents() -> Result<Vec<HerdrAgent>, String> {
    list_agents_with(&herdr_binary())
}

fn list_agents_with(binary: &OsStr) -> Result<Vec<HerdrAgent>, String> {
    let output = Command::new(binary)
        .args(["pane", "list"])
        .output()
        .map_err(|error| format!("Could not list Herdr panes: {error}"))?;
    if !output.status.success() {
        return Err(command_error("Could not list Herdr panes", &output.stderr));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Herdr returned invalid pane JSON: {error}"))?;
    let own_pane = env::var("HERDR_PANE_ID").ok();
    let own_workspace = env::var("HERDR_WORKSPACE_ID").ok();
    let mut agents = parse_agents(&value);
    if let Some(own_workspace) = own_workspace.as_deref() {
        agents.sort_by_key(|agent| (agent.workspace != own_workspace) as u8);
    }
    let spaces = Command::new(binary)
        .args(["workspace", "list"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok());
    if let Some(spaces) = spaces
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/result/workspaces")
                .or_else(|| value.get("workspaces"))
        })
        .and_then(Value::as_array)
    {
        for agent in &mut agents {
            if let Some(label) = spaces
                .iter()
                .find(|space| space["workspace_id"].as_str() == Some(agent.workspace.as_str()))
                .and_then(|space| space["label"].as_str())
            {
                agent.workspace = label.to_owned();
            }
        }
    }
    Ok(agents
        .into_iter()
        .filter(|agent| own_pane.as_deref() != Some(agent.pane_id.as_str()))
        .collect())
}

pub fn send_message(pane_id: &str, message: &str) -> Result<(), String> {
    send_message_with(&herdr_binary(), pane_id, message)
}

fn send_message_with(binary: &OsStr, pane_id: &str, message: &str) -> Result<(), String> {
    let output = Command::new(binary)
        .args(["pane", "send-text", pane_id, message])
        .output()
        .map_err(|error| format!("Could not send the agent request: {error}"))?;
    if !output.status.success() {
        return Err(command_error(
            "Could not send the agent request",
            &output.stderr,
        ));
    }
    let output = Command::new(binary)
        .args(["pane", "send-keys", pane_id, "enter"])
        .output()
        .map_err(|error| format!("Could not submit the agent request: {error}"))?;
    if !output.status.success() {
        return Err(command_error(
            "Could not submit the agent request",
            &output.stderr,
        ));
    }
    Ok(())
}

pub fn send_agent_request(
    pane_id: &str,
    root: &Path,
    prompt: &str,
    amend: bool,
) -> Result<(), String> {
    send_message(pane_id, &agent_request(root, prompt, amend))
}

fn herdr_command() -> Command {
    Command::new(herdr_binary())
}

fn herdr_binary() -> OsString {
    env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"))
}

fn command_error(prefix: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {detail}")
    }
}

fn git_panes_in_tab(value: &Value, target_pane: &str) -> Vec<String> {
    let Some(panes) = value
        .pointer("/result/panes")
        .or_else(|| value.get("panes"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let Some(tab_id) = panes
        .iter()
        .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(target_pane))
        .and_then(|pane| pane.get("tab_id"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };

    panes
        .iter()
        .filter(|pane| {
            pane.get("tab_id").and_then(Value::as_str) == Some(tab_id)
                && pane.get("label").and_then(Value::as_str) == Some(PANE_TITLE)
        })
        .filter_map(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(values) => {
            for key in keys {
                if let Some(value) = values.get(*key).and_then(|value| value.as_str()) {
                    return Some(value.to_owned());
                }
            }
            values.values().find_map(|value| find_string(value, keys))
        }
        Value::Array(values) => values.iter().find_map(|value| find_string(value, keys)),
        _ => None,
    }
}

fn parse_agents(value: &Value) -> Vec<HerdrAgent> {
    let Some(panes) = value
        .pointer("/result/panes")
        .or_else(|| value.get("panes"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    panes
        .iter()
        .filter_map(|pane| {
            let pane_id = pane.get("pane_id")?.as_str()?.to_owned();
            let agent = pane
                .get("agent")
                .and_then(Value::as_str)
                .or_else(|| pane.pointer("/agent_session/agent").and_then(Value::as_str))?;
            if agent.is_empty() || agent == "unknown" {
                return None;
            }
            let title = pane
                .get("terminal_title_stripped")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty());
            Some(HerdrAgent {
                pane_id,
                workspace: pane["workspace_id"].as_str().unwrap_or_default().into(),
                agent_type: agent.to_owned(),
                status: pane["agent_status"].as_str().unwrap_or("unknown").into(),
                session: pane
                    .pointer("/agent_session/value")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                label: title.unwrap_or(agent).to_owned(),
                cwd: pane
                    .get("foreground_cwd")
                    .or_else(|| pane.get("cwd"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::path::Path;

    use super::{
        HerdrAgent, agent_request, find_string, git_panes_in_tab, parse_agents, send_message_with,
    };

    #[test]
    fn amend_agent_request_is_explicit() {
        assert_eq!(
            agent_request(Path::new("/repo"), "Improve the message", false),
            "Commit the staged changes in the repository below.\nRepository: /repo\n\nAdditional request:\nImprove the message"
        );
        assert!(
            agent_request(Path::new("/repo"), "Improve the message", true)
                .starts_with("Amend the current HEAD")
        );
        assert_eq!(
            agent_request(Path::new("/repo"), "", false),
            "Commit the staged changes in the repository below.\nRepository: /repo"
        );
    }

    #[test]
    fn parses_only_agent_panes_from_herdr_output() {
        let value = json!({"result": {"panes": [
            {"pane_id": "w1:p1", "agent": "codex", "terminal_title_stripped": "Review agent", "foreground_cwd": "/repo"},
            {"pane_id": "w1:p2", "label": "Git", "cwd": "/repo"}
        ]}});
        assert_eq!(
            parse_agents(&value),
            vec![HerdrAgent {
                pane_id: "w1:p1".to_owned(),
                label: "Review agent".to_owned(),
                cwd: "/repo".to_owned(),
                agent_type: "codex".into(),
                status: "unknown".into(),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn finds_nested_invocation_values() {
        let context = json!({
            "workspace": {"cwd": "/repo"},
            "focused_pane": {"pane_id": "w1:p2"}
        });

        assert_eq!(
            find_string(&context, &["pane_id"]),
            Some("w1:p2".to_owned())
        );
        assert_eq!(find_string(&context, &["cwd"]), Some("/repo".to_owned()));
    }

    #[test]
    fn finds_every_git_pane_in_the_invoking_tab_only() {
        let panes = json!({"result": {"panes": [
            {"pane_id": "w1:p1", "tab_id": "w1:t1"},
            {"pane_id": "w1:p2", "tab_id": "w1:t1", "label": "Git"},
            {"pane_id": "w1:p3", "tab_id": "w1:t1", "label": "Git"},
            {"pane_id": "w1:p4", "tab_id": "w1:t2", "label": "Git"},
            {"pane_id": "w1:p5", "tab_id": "w1:t1", "label": "Build"}
        ]}});

        assert_eq!(
            git_panes_in_tab(&panes, "w1:p1"),
            ["w1:p2".to_owned(), "w1:p3".to_owned()]
        );
        assert_eq!(
            git_panes_in_tab(&panes, "w1:p2"),
            ["w1:p2".to_owned(), "w1:p3".to_owned()]
        );
        assert!(git_panes_in_tab(&panes, "missing").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn send_failures_name_the_agent_request() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("herdr-git-fake-herdr-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let herdr = dir.join("herdr");
        fs::write(
            &herdr,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$3\" = \"w1:gone\" ]; then echo \"pane w1:gone not found\" >&2; exit 1; fi\n",
                "case \"$2\" in\n",
                "  send-text) exit 0 ;;\n",
                "  *) echo \"keys rejected\" >&2; exit 1 ;;\n",
                "esac\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            send_message_with(herdr.as_os_str(), "w1:gone", "Commit the staged changes"),
            Err("Could not send the agent request: pane w1:gone not found".to_owned())
        );
        assert_eq!(
            send_message_with(herdr.as_os_str(), "w1:p9", "Commit the staged changes"),
            Err("Could not submit the agent request: keys rejected".to_owned())
        );
        let missing = dir.join("missing-herdr");
        let error = send_message_with(missing.as_os_str(), "w1:p9", "hello").unwrap_err();
        assert!(
            error.starts_with("Could not send the agent request: "),
            "{error}"
        );

        fs::remove_dir_all(dir).unwrap();
    }
}
