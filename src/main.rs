//! Herdr plugin entry point: set `$pr_open` / `$pr_merged` / `$pr_closed` on
//! every Claude Code or Codex pane from the PRs its session touched.
//!
//! Herdr runs this at server startup and on `pane.agent_status_changed`.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use herdr_sim::{Cache, PrState, claude_prs, codex_prs, token_value};
use serde_json::Value;

fn main() -> Result<()> {
    let herdr = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
    let state_dir = env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("herdr-sim"));
    std::fs::create_dir_all(&state_dir)?;
    let cache_path = state_dir.join("pr-cache.json");
    let mut cache = Cache::load(&cache_path);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let list: Value = serde_json::from_slice(&run(&herdr, &["agent", "list"])?)?;
    for agent in list["result"]["agents"].as_array().into_iter().flatten() {
        let (Some(pane), Some(session)) = (
            agent["pane_id"].as_str(),
            agent["agent_session"]["value"].as_str(),
        ) else {
            continue;
        };
        let urls = match agent["agent"].as_str() {
            Some("claude") => claude_prs(&home, session, &mut cache),
            Some("codex") => codex_prs(&home, session, &mut cache),
            _ => continue,
        };

        let mut counts: HashMap<PrState, usize> = HashMap::new();
        for url in &urls {
            if let Some(state) = cache.state(url, now, gh_pr_state) {
                *counts.entry(state).or_default() += 1;
            }
        }

        let mut args: Vec<String> = ["pane", "report-metadata", pane, "--source", "herdr-sim"]
            .map(String::from)
            .to_vec();
        for state in PrState::ALL {
            match token_value(state, counts.get(&state).copied().unwrap_or(0)) {
                Some(value) => {
                    args.extend(["--token".into(), format!("{}={value}", state.token())])
                }
                None => args.extend(["--clear-token".into(), state.token().into()]),
            }
        }
        // A pane can close between `agent list` and here; keep going for the rest.
        if let Err(err) = run(&herdr, &args) {
            eprintln!("herdr-sim: {pane}: {err:#}");
        }
    }

    cache.save(&cache_path)?;
    Ok(())
}

fn gh_pr_state(url: &str) -> Option<PrState> {
    let out = Command::new("gh")
        .args(["pr", "view", url, "--json", "state"])
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    PrState::parse(v["state"].as_str()?)
}

fn run(program: &str, args: &[impl AsRef<std::ffi::OsStr> + std::fmt::Debug]) -> Result<Vec<u8>> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.stdout)
}
