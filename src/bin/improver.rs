//! The improver: a second GDK agent that edits this crate to raise the
//! benchmark score.
//!
//!     MC_CONSOLE=fifo:/run/mc-agents-server.stdin cargo run --release --bin improver -- --generations 3
//!
//! Its only tools are read_file and write_file (inside src/, never src/bin/,
//! where the benchmark and the improver live), cargo_build, run_bench,
//! git_diff, git_commit and git_revert. It has no shell. Every command it can
//! cause is fixed here.
//!
//! It works on an `improver/...` branch. git_commit refuses unless the
//! benchmark ran on exactly the current change and beat the best score, so the
//! branch holds only commits that raised the score. Each generation's result
//! goes to bench/scores.csv. Uncommitted changes are reverted after each
//! generation.

use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use mc_agents::{
    brain::{
        describe_event,
        tools::{object_schema, GdkTools, ToolDef, ToolSet},
        truncate, Llm, Tier, Turn,
    },
    config::Config,
    log::{self, Kind},
};
use serde_json::{json, Value};
use tokio::process::Command;

const WHO: &str = "improver";
const CSV: &str = "bench/scores.csv";

#[derive(Default)]
struct State {
    best: f64,
    /// Hash of `git diff -- src` that the last benchmark ran on, and its result.
    last_bench: Option<(String, f64, Value)>,
    built: Option<String>,
    committed: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut generations = 3;
    let mut bench_args: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--generations" => generations = args.next().context("--generations needs a number")?.parse()?,
            "--bench-args" => {
                bench_args = args.next().context("--bench-args needs a value")?.split_whitespace().map(String::from).collect()
            }
            other => bail!("unknown argument {other}"),
        }
    }
    log::init("logs/improver")?;
    let config = Config::load("config.toml")?;
    let llm = Llm::from_config(&config.llm)?.with_timeout(Duration::from_secs(3 * 3600));
    let root = std::env::current_dir()?.canonicalize()?;
    if !root.join("src/bin/improver.rs").exists() {
        bail!("run the improver from the mc-agents crate root");
    }

    if !git(&root, &["status", "--porcelain", "--", "src"]).await?.trim().is_empty() {
        bail!("src/ has uncommitted changes; commit or stash them first");
    }
    let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await?.trim().to_string();
    let branch = if branch.starts_with("improver/") {
        branch
    } else {
        let name = format!("improver/{}", chrono::Local::now().format("%Y%m%d-%H%M"));
        git(&root, &["switch", "-c", &name]).await?;
        name
    };
    log::log(WHO, Kind::Info, format!("working on branch {branch}"));

    let state = Arc::new(Mutex::new(State::default()));
    log::log(WHO, Kind::Plan, "building and running the baseline benchmark");
    let build = cargo_build(&root).await;
    if let Err(e) = build {
        bail!("the baseline does not build: {e}");
    }
    let (baseline, report) = bench(&root, &bench_args).await?;
    {
        let mut s = state.lock().unwrap();
        s.best = baseline;
        s.last_bench = Some((diff_hash(&root).await?, baseline, report.clone()));
    }
    record(0, baseline, baseline, false, "", "baseline")?;
    log::log(WHO, Kind::Result, format!("baseline score {baseline:.1}"));

    let tools = tool_set(root.clone(), bench_args.clone(), state.clone());
    for generation in 1..=generations {
        let (best, report, commits_before) = {
            let s = state.lock().unwrap();
            (s.best, s.last_bench.as_ref().map(|b| b.2.clone()).unwrap_or_default(), s.committed.len())
        };
        let history = std::fs::read_to_string(CSV).unwrap_or_default();
        let failures = recent_failures();
        let turn = Turn {
            system: SYSTEM.into(),
            user: format!(
                "Generation {generation} of {generations}. Best score so far: {best:.1}.\n\n\
                 Last benchmark report:\n{report}\n\nScore history (CSV):\n{history}\n\
                 Recent failures from the benchmark log:\n{failures}\n\n\
                 Make one focused improvement, then build, benchmark, and commit it or revert it."
            ),
            tools: Arc::new(GdkTools::all(tools.clone())),
            tier: Tier::Strong,
            max_inferences: 40,
            stop_when: None,
        };
        log::log(WHO, Kind::Think, format!("generation {generation}: thinking"));
        let outcome = llm
            .run_turn(turn, |e| {
                if let Some(line) = describe_event(e) {
                    log::log(WHO, Kind::Llm, truncate(&line, 240));
                }
            })
            .await;
        if let Err(e) = &outcome {
            log::log(WHO, Kind::Fail, format!("generation {generation} turn failed: {e:#}"));
        }
        // Whatever was not committed is thrown away.
        let leftover = !git(&root, &["status", "--porcelain", "--", "src"]).await?.trim().is_empty();
        if leftover {
            revert(&root).await?;
            log::log(WHO, Kind::Info, "reverted uncommitted changes");
        }
        let s = state.lock().unwrap();
        let kept = s.committed.len() > commits_before;
        let last_score = s.last_bench.as_ref().map(|b| b.1).unwrap_or(s.best);
        let commit = if kept { s.committed.last().cloned().unwrap_or_default() } else { String::new() };
        let summary = if kept { "kept" } else { "reverted" };
        record(generation, last_score, s.best, kept, &commit, summary)?;
        log::log(
            WHO,
            if kept { Kind::Result } else { Kind::Info },
            format!("generation {generation}: score {last_score:.1}, best {:.1}, {summary}", s.best),
        );
    }
    let s = state.lock().unwrap();
    log::log(
        WHO,
        Kind::Result,
        format!(
            "done: best {:.1} (baseline {baseline:.1}), {} commit(s) on {branch}. Review, then merge it with: git merge {branch}",
            s.best,
            s.committed.len()
        ),
    );
    Ok(())
}

const SYSTEM: &str = "\
You improve mc-agents, a Rust crate of self-improving Minecraft bots built on goose's GDK and azalea. \
A benchmark plays timed scenarios (gather logs, craft a table, craft a pickaxe, mine stone, sleep) \
through the real agent loop and scores them: 100 per pass, up to 50 for speed, minus 2 per model call.

Tools: read_file (a file or directory under src/), write_file (whole files under src/, not src/bin/), \
cargo_build, run_bench, git_diff, git_commit, git_revert. There is no shell.

Work like this:
1. Read the benchmark report and the failures. Read the code that matters: src/agent/ (the loop, \
planning, prompts), src/body/ (the actions), src/skills.rs (the Rhai sandbox).
2. Make ONE focused change likely to raise the score: clearer tool errors, a better prompt, a fix in \
an action that fails, fewer wasted model calls.
3. cargo_build. Fix compile errors.
4. run_bench. It takes several minutes.
5. If the score beat the best, git_commit with a short imperative subject line and a body saying why. \
Otherwise git_revert. git_commit refuses changes that did not beat the best score.

Rules: keep every guardrail (the Rhai sandbox limits, the model-call budgets, skill retry caps, path \
checks). Never weaken a check to pass a test. Never log or print secrets. Write whole files, keeping \
everything you are not changing.";

fn tool_set(root: PathBuf, bench_args: Vec<String>, state: Arc<Mutex<State>>) -> Arc<ToolSet> {
    let set = ToolSet::new();
    {
        let root = root.clone();
        set.register(ToolDef::new(
            "read_file",
            "Read a file under src/, or list a directory under src/.",
            object_schema(&[("path", "string", "path relative to the crate root, e.g. src/agent/mod.rs")]),
            move |args| {
                let root = root.clone();
                async move {
                    let path = checked_path(&root, args["path"].as_str().unwrap_or_default(), false)?;
                    if path.is_dir() {
                        let mut names: Vec<String> = std::fs::read_dir(&path)
                            .map_err(|e| e.to_string())?
                            .flatten()
                            .map(|e| {
                                let name = e.file_name().to_string_lossy().to_string();
                                if e.path().is_dir() { format!("{name}/") } else { name }
                            })
                            .collect();
                        names.sort();
                        return Ok(names.join("\n"));
                    }
                    std::fs::read_to_string(&path).map_err(|e| format!("cannot read: {e}"))
                }
            },
        ));
    }
    {
        let (root, state) = (root.clone(), state.clone());
        set.register(ToolDef::new(
            "write_file",
            "Replace a whole .rs file under src/ (not src/bin/). Creates it if missing.",
            object_schema(&[
                ("path", "string", "path relative to the crate root"),
                ("content", "string", "the complete new file"),
            ]),
            move |args| {
                let (root, state) = (root.clone(), state.clone());
                async move {
                    let path = checked_path(&root, args["path"].as_str().unwrap_or_default(), true)?;
                    let content = args["content"].as_str().ok_or("content is required")?;
                    if content.len() > 200_000 {
                        return Err("file too large".into());
                    }
                    std::fs::write(&path, content).map_err(|e| format!("cannot write: {e}"))?;
                    state.lock().unwrap().built = None;
                    log::log(WHO, Kind::Tool, format!("wrote {}", path.strip_prefix(&root).unwrap_or(&path).display()));
                    Ok(format!("wrote {} bytes", content.len()))
                }
            },
        ));
    }
    {
        let (root, state) = (root.clone(), state.clone());
        set.register(ToolDef::new(
            "cargo_build",
            "Build the crate. Returns ok, or the compiler errors.",
            object_schema(&[]),
            move |_| {
                let (root, state) = (root.clone(), state.clone());
                async move {
                    cargo_build(&root).await?;
                    let hash = diff_hash(&root).await.map_err(|e| e.to_string())?;
                    state.lock().unwrap().built = Some(hash);
                    Ok("ok".into())
                }
            },
        ));
    }
    {
        let (root, state) = (root.clone(), state.clone());
        set.register(ToolDef::new(
            "run_bench",
            "Run the benchmark on the built code (takes minutes). Returns the score report.",
            object_schema(&[]),
            move |_| {
                let (root, state, bench_args) = (root.clone(), state.clone(), bench_args.clone());
                async move {
                    let hash = diff_hash(&root).await.map_err(|e| e.to_string())?;
                    if state.lock().unwrap().built.as_deref() != Some(hash.as_str()) {
                        return Err("the code changed since the last successful cargo_build; build first".into());
                    }
                    let (score, report) = bench(&root, &bench_args).await.map_err(|e| format!("{e:#}"))?;
                    let mut s = state.lock().unwrap();
                    s.last_bench = Some((hash, score, report.clone()));
                    let verdict = if score > s.best { "beats the best: commit it" } else { "does not beat the best: revert it" };
                    Ok(json!({"score": score, "best": s.best, "verdict": verdict, "report": report}).to_string())
                }
            },
        ));
    }
    {
        let root = root.clone();
        set.register(ToolDef::new(
            "git_diff",
            "Show the uncommitted changes under src/.",
            object_schema(&[]),
            move |_| {
                let root = root.clone();
                async move {
                    let diff = git(&root, &["diff", "--", "src"]).await.map_err(|e| e.to_string())?;
                    Ok(if diff.is_empty() { "no changes".into() } else { truncate(&diff, 12_000) })
                }
            },
        ));
    }
    {
        let (root, state) = (root.clone(), state.clone());
        set.register(ToolDef::new(
            "git_commit",
            "Commit the changes under src/. Only allowed if run_bench ran on exactly these changes and beat the best score.",
            object_schema(&[("message", "string", "commit message: imperative subject under 50 chars, blank line, body")]),
            move |args| {
                let (root, state) = (root.clone(), state.clone());
                async move {
                    let message = args["message"].as_str().unwrap_or_default().trim().to_string();
                    if message.is_empty() {
                        return Err("message is required".into());
                    }
                    let hash = diff_hash(&root).await.map_err(|e| e.to_string())?;
                    let (score, best) = {
                        let s = state.lock().unwrap();
                        match &s.last_bench {
                            Some((h, score, _)) if *h == hash => (*score, s.best),
                            _ => return Err("run_bench has not run on these exact changes".into()),
                        }
                    };
                    if git(&root, &["status", "--porcelain", "--", "src"]).await.map_err(|e| e.to_string())?.trim().is_empty() {
                        return Err("nothing to commit".into());
                    }
                    if score <= best {
                        return Err(format!("score {score:.1} does not beat the best {best:.1}; git_revert instead"));
                    }
                    let full = format!("{message}\n\nBenchmark score {best:.1} -> {score:.1}.");
                    git(&root, &["add", "--", "src"]).await.map_err(|e| e.to_string())?;
                    git(&root, &["-c", "user.name=mc-agents improver", "-c", "user.email=improver@mc-agents.local", "commit", "-q", "-m", &full])
                        .await
                        .map_err(|e| e.to_string())?;
                    let commit = git(&root, &["rev-parse", "--short", "HEAD"]).await.map_err(|e| e.to_string())?.trim().to_string();
                    let mut s = state.lock().unwrap();
                    s.best = score;
                    s.committed.push(commit.clone());
                    log::log(WHO, Kind::Result, format!("committed {commit}: {}", message.lines().next().unwrap_or_default()));
                    Ok(format!("committed {commit}; new best {score:.1}"))
                }
            },
        ));
    }
    {
        let (root, state) = (root.clone(), state.clone());
        set.register(ToolDef::new(
            "git_revert",
            "Throw away all uncommitted changes under src/.",
            object_schema(&[]),
            move |_| {
                let (root, state) = (root.clone(), state.clone());
                async move {
                    revert(&root).await.map_err(|e| e.to_string())?;
                    state.lock().unwrap().built = None;
                    Ok("reverted".into())
                }
            },
        ));
    }
    set
}

/// Resolves a path and makes sure it stays inside src/ (and out of src/bin/
/// for writes), even through symlinks.
fn checked_path(root: &Path, path: &str, write: bool) -> Result<PathBuf, String> {
    let relative = Path::new(path.trim().trim_start_matches("./"));
    if relative.is_absolute() || relative.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("use a plain relative path under src/, without .. or a leading /".into());
    }
    let src = root.join("src");
    let full = root.join(relative);
    let resolved = if full.exists() {
        full.canonicalize().map_err(|e| e.to_string())?
    } else {
        let parent = full.parent().ok_or("bad path")?.canonicalize().map_err(|_| "the parent directory does not exist")?;
        parent.join(full.file_name().ok_or("bad path")?)
    };
    if !resolved.starts_with(&src) {
        return Err("only files under src/ are allowed".into());
    }
    if write {
        if resolved.starts_with(src.join("bin")) {
            return Err("src/bin/ holds the benchmark and the improver; it is read-only".into());
        }
        if resolved.extension().is_none_or(|e| e != "rs") {
            return Err("only .rs files can be written".into());
        }
        if resolved.is_symlink() {
            return Err("symlinks cannot be written".into());
        }
    }
    Ok(resolved)
}

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(root)
        .output()
        .await?;
    if !output.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn diff_hash(root: &Path) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let diff = git(root, &["diff", "--", "src"]).await?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "--", "src"]).await?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    diff.hash(&mut hasher);
    for file in untracked.lines() {
        file.hash(&mut hasher);
        std::fs::read(root.join(file)).unwrap_or_default().hash(&mut hasher);
    }
    Ok(format!("{:x}", hasher.finish()))
}

async fn revert(root: &Path) -> Result<()> {
    git(root, &["checkout", "--", "src"]).await?;
    git(root, &["clean", "-fdq", "--", "src"]).await?;
    Ok(())
}

async fn cargo_build(root: &Path) -> Result<(), String> {
    log::log(WHO, Kind::Tool, "cargo build");
    let run = Command::new("cargo")
        .args(["build", "--release", "--bin", "bench", "--bin", "mc-agents", "--message-format", "short"])
        .current_dir(root)
        .stdin(Stdio::null())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(1200), run)
        .await
        .map_err(|_| "cargo build took longer than 20 minutes".to_string())?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains("error") || l.contains("warning: unused")).take(40).collect();
    log::log(WHO, Kind::Fail, format!("build failed: {} error lines", errors.len()));
    Err(format!("build failed:\n{}", errors.join("\n")))
}

/// Runs the benchmark binary and returns its score and report.
async fn bench(root: &Path, args: &[String]) -> Result<(f64, Value)> {
    log::log(WHO, Kind::Tool, "running the benchmark");
    let run = Command::new(root.join("target/release/bench"))
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(3600), run).await.context("the benchmark took over an hour")??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("BENCH_SCORE "))
        .context("the benchmark printed no score; is the server up and MC_CONSOLE set?")?;
    let report: Value = serde_json::from_str(line)?;
    let score = report["score"].as_f64().context("no score in the report")?;
    log::log(WHO, Kind::Result, format!("benchmark score {score:.1}"));
    // The report the model sees: per-scenario lines, not the whole log.
    let summary = json!({
        "score": score,
        "passed": report["passed"],
        "scenarios": report["scenarios"],
    });
    Ok((score, summary))
}

/// The last failure lines of the benchmark bot's log, for the model.
fn recent_failures() -> String {
    let log = std::fs::read_to_string("logs/bench/Bench.log").unwrap_or_default();
    let lines: Vec<&str> = log.lines().filter(|l| l.contains(" fail ") || l.contains("step_failed")).collect();
    let tail = &lines[lines.len().saturating_sub(25)..];
    if tail.is_empty() { "none".into() } else { tail.iter().map(|l| truncate(l, 220)).collect::<Vec<_>>().join("\n") }
}

fn record(generation: u32, score: f64, best: f64, kept: bool, commit: &str, summary: &str) -> Result<()> {
    let new = !Path::new(CSV).exists();
    let mut text = if new { "generation,finished,score,best,kept,commit,summary\n".to_string() } else { String::new() };
    text.push_str(&format!(
        "{generation},{},{score:.1},{best:.1},{kept},{commit},{}\n",
        chrono::Local::now().to_rfc3339(),
        summary.replace(',', ";")
    ));
    use std::io::Write;
    std::fs::OpenOptions::new().create(true).append(true).open(CSV)?.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_in_src_and_out_of_bin() {
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(checked_path(&root, "src/agent/mod.rs", true).is_ok());
        assert!(checked_path(&root, "src/agent/new_file.rs", true).is_ok());
        assert!(checked_path(&root, "src", false).is_ok());
        for bad in ["src/../config.toml", "/etc/passwd", "Cargo.toml", ".env", "src/bin/bench.rs", "src/agent/x.sh"] {
            assert!(checked_path(&root, bad, true).is_err(), "{bad} should be refused");
        }
        assert!(checked_path(&root, "src/bin/bench.rs", false).is_ok());
    }
}
