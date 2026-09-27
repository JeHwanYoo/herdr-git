use std::{env, fs, io::ErrorKind, path::PathBuf, process::Command};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{PLUGIN_ID, herdr_command};

#[derive(Clone, Debug)]
struct Rect {
    id: String,
    x: u64,
    y: u64,
    w: u64,
    h: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Step {
    pane: String,
    target: String,
    direction: String,
    ratio: f64,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    tab: String,
    parking: String,
    placeholder: String,
    parked: Vec<String>,
    steps: Vec<Step>,
    git: Option<String>,
}

fn call(command: &mut Command, action: &str) -> Result<Value, String> {
    let output = command.output().map_err(|e| format!("{action}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("{action}: {e}"))
}

fn field<'a>(value: &'a Value, path: &str) -> Result<&'a str, String> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Herdr response missing {path}"))
}

fn number(value: &Value, path: &str) -> Result<u64, String> {
    value
        .pointer(path)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("Herdr response missing {path}"))
}

fn layout(pane: &str) -> Result<(String, String, Vec<Rect>), String> {
    let output = call(
        herdr_command().args(["pane", "layout", "--pane", pane]),
        "read layout",
    )?;
    let value = output
        .pointer("/result/layout")
        .ok_or("Herdr response missing layout")?;
    let workspace = field(value, "/workspace_id")?.to_owned();
    let tab = field(value, "/tab_id")?.to_owned();
    let x0 = number(value, "/area/x")?;
    let y0 = number(value, "/area/y")?;
    let panes = value
        .pointer("/panes")
        .and_then(Value::as_array)
        .ok_or("Herdr response missing panes")?;
    let rects = panes
        .iter()
        .map(|pane| {
            Ok(Rect {
                id: field(pane, "/pane_id")?.to_owned(),
                x: number(pane, "/rect/x")?
                    .checked_sub(x0)
                    .ok_or("pane outside layout")?,
                y: number(pane, "/rect/y")?
                    .checked_sub(y0)
                    .ok_or("pane outside layout")?,
                w: number(pane, "/rect/width")?,
                h: number(pane, "/rect/height")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((workspace, tab, rects))
}

fn plan(rects: &[Rect]) -> Result<(String, Vec<Step>), String> {
    if rects.len() == 1 {
        return Ok((rects[0].id.clone(), vec![]));
    }
    if rects.is_empty() {
        return Err("tab has no panes".into());
    }
    for horizontal in [true, false] {
        let start = |r: &Rect| if horizontal { r.x } else { r.y };
        let end = |r: &Rect| start(r) + if horizontal { r.w } else { r.h };
        let lo = rects.iter().map(start).min().unwrap();
        let hi = rects.iter().map(end).max().unwrap();
        let mut edges: Vec<_> = rects
            .iter()
            .map(end)
            .filter(|e| *e > lo + 2 && *e + 2 < hi)
            .collect();
        edges.sort_unstable();
        edges.dedup();
        for edge in edges {
            let before = rects.iter().filter(|r| end(r) <= edge).map(end).max();
            let after = rects.iter().filter(|r| end(r) > edge).map(start).min();
            if !matches!((before, after), (Some(b), Some(a)) if a >= b) {
                continue;
            }
            let (left, right): (Vec<_>, Vec<_>) =
                rects.iter().cloned().partition(|r| end(r) <= edge);
            let (head_left, steps_left) = plan(&left)?;
            let (head_right, steps_right) = plan(&right)?;
            let mut steps = vec![Step {
                pane: head_right,
                target: head_left.clone(),
                direction: if horizontal { "right" } else { "down" }.into(),
                ratio: (edge - lo) as f64 / (hi - lo) as f64,
            }];
            steps.extend(steps_left);
            steps.extend(steps_right);
            return Ok((head_left, steps));
        }
    }
    Err("pane layout cannot be safely rearranged".into())
}

fn state_path(tab: &str) -> Result<PathBuf, String> {
    let root = env::var_os("HERDR_PLUGIN_STATE_DIR").ok_or("HERDR_PLUGIN_STATE_DIR is not set")?;
    let name: String = tab
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(PathBuf::from(root).join(format!("layout-{name}.json")))
}

fn save(state: &Pending) -> Result<(), String> {
    let path = state_path(&state.tab)?;
    fs::create_dir_all(path.parent().ok_or("invalid state directory")?)
        .map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(state).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())
}

fn load(tab: &str) -> Result<Option<Pending>, String> {
    match fs::read(state_path(tab)?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn pane_tab(pane: &str) -> Result<String, String> {
    let output = call(herdr_command().args(["pane", "get", pane]), "read pane")?;
    Ok(field(&output, "/result/pane/tab_id")?.to_owned())
}

fn move_pane(pane: &str, tab: &str, step: &Step) -> Result<(), String> {
    call(
        herdr_command().args([
            "pane",
            "move",
            pane,
            "--tab",
            tab,
            "--split",
            &step.direction,
            "--target-pane",
            &step.target,
            "--ratio",
            &step.ratio.to_string(),
            "--no-focus",
        ]),
        "move pane",
    )?;
    Ok(())
}

fn close_pane(pane: &str) -> Result<(), String> {
    call(herdr_command().args(["pane", "close", pane]), "close pane")?;
    Ok(())
}

fn git_in_tab(tab: &str) -> Result<Option<String>, String> {
    let output = call(herdr_command().args(["pane", "list"]), "list panes")?;
    Ok(output
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .and_then(|panes| {
            panes.iter().find_map(|pane| {
                (pane["tab_id"].as_str() == Some(tab) && pane["label"].as_str() == Some("Git"))
                    .then(|| pane["pane_id"].as_str())
                    .flatten()
                    .map(str::to_owned)
            })
        }))
}

pub(super) fn recover(tab: &str) -> Result<(), String> {
    let Some(state) = load(tab)? else {
        return Ok(());
    };
    let mut needs_restore = false;
    for pane in &state.parked {
        if pane_tab(pane)? == state.parking {
            needs_restore = true;
        }
    }
    if needs_restore {
        let git = match state.git {
            Some(git) => Some(git),
            None => git_in_tab(tab)?,
        };
        if let Some(git) = git {
            close_pane(&git)?;
        }
        for step in &state.steps {
            if pane_tab(&step.pane)? == state.parking {
                move_pane(&step.pane, tab, step)?;
            }
        }
    }
    let _ = close_pane(&state.placeholder);
    fs::remove_file(state_path(tab)?).map_err(|e| e.to_string())
}

fn open_plugin(target: Option<&str>, cwd: Option<&str>) -> Result<String, String> {
    let mut command = herdr_command();
    command.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN_ID,
        "--entrypoint",
        "git",
        "--placement",
        "split",
        "--direction",
        "right",
        "--focus",
    ]);
    if let Some(target) = target {
        command.args(["--target-pane", target]);
    }
    if let Some(cwd) = cwd {
        command.args(["--cwd", cwd]);
    }
    let output = call(&mut command, "open Git pane")?;
    Ok(field(&output, "/result/plugin_pane/pane/pane_id")?.to_owned())
}

pub(super) fn open(target: Option<&str>, cwd: Option<&str>) -> Result<(), String> {
    let Some(target) = target else {
        open_plugin(None, cwd)?;
        return Ok(());
    };
    let (workspace, tab, mut rects) = layout(target)?;
    if load(&tab)?.is_some() {
        recover(&tab)?;
        rects = layout(target)?.2;
    }
    let (anchor, steps) = plan(&rects)?;
    if steps.is_empty() {
        open_plugin(Some(&anchor), cwd)?;
        return Ok(());
    }
    let created = call(
        herdr_command().args(["tab", "create", "--workspace", &workspace, "--no-focus"]),
        "create parking tab",
    )?;
    let parking = field(&created, "/result/tab/tab_id")?.to_owned();
    let placeholder = field(&created, "/result/root_pane/pane_id")?.to_owned();
    let mut state = Pending {
        tab: tab.clone(),
        parking: parking.clone(),
        placeholder,
        parked: rects
            .iter()
            .filter(|r| r.id != anchor)
            .map(|r| r.id.clone())
            .collect(),
        steps,
        git: None,
    };
    if let Err(error) = save(&state) {
        let _ = close_pane(&state.placeholder);
        return Err(error);
    }
    let result = (|| {
        for pane in &state.parked {
            call(
                herdr_command().args([
                    "pane",
                    "move",
                    pane,
                    "--tab",
                    &parking,
                    "--split",
                    "right",
                    "--no-focus",
                ]),
                "park pane",
            )?;
        }
        let git = open_plugin(Some(&anchor), cwd)?;
        state.git = Some(git.clone());
        save(&state)?;
        for step in &state.steps {
            move_pane(&step.pane, &tab, step)?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = result {
        return Err(match recover(&tab) {
            Ok(()) => error,
            Err(restore) => format!("{error}; layout recovery failed: {restore}"),
        });
    }
    fs::remove_file(state_path(&tab)?).map_err(|e| e.to_string())?;
    let _ = close_pane(&state.placeholder);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: &str, x: u64, y: u64, w: u64, h: u64) -> Rect {
        Rect {
            id: id.into(),
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn restores_stacked_panes_under_a_root_right_split() {
        let (anchor, steps) = plan(&[
            r("top", 0, 0, 100, 50),
            r("bottom-left", 0, 51, 50, 49),
            r("bottom-right", 51, 51, 49, 49),
        ])
        .unwrap();
        assert_eq!(anchor, "top");
        assert_eq!(steps.len(), 2);
        assert_eq!(
            (&*steps[0].pane, &*steps[0].target, &*steps[0].direction),
            ("bottom-left", "top", "down")
        );
        assert_eq!(
            (&*steps[1].pane, &*steps[1].target, &*steps[1].direction),
            ("bottom-right", "bottom-left", "right")
        );
    }
}
