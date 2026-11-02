//! `swarm` — the CLI entry point.
//!
//! Mirrors Go `cmd/swarm/main.go`: `init`, `seed`, `run`, `board`, `status`,
//! `log`.
//!
//! The `run` guard that rejects a `-`-prefixed directory is kept verbatim: without
//! it `swarm run --help` silently created `./--help/.swarm/worktrees/wN`,
//! polluting the worktree registry with a second set of paths and making
//! syncWorktree misbehave (workers claimed items but never called the LLM).

use std::path::PathBuf;
use std::process::ExitCode;

use swarm_config::{parse_run_args, ParseError};
use swarm_git::Repo;

const SWARM_DIR: &str = ".swarm";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        eprintln!("usage: swarm <command> [args]");
        eprintln!("commands: init, seed, run, board, status, log");
        return ExitCode::FAILURE;
    };

    let rest = &args[1..];
    match cmd.as_str() {
        "init" => match block_on(cmd_init(rest)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("init: {e}");
                ExitCode::FAILURE
            }
        },
        "seed" => match block_on(cmd_seed(rest)) {
            Ok(n) => {
                println!("seeded {n} item(s)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("seed: {e}");
                ExitCode::FAILURE
            }
        },
        "run" => cmd_run(rest),
        "board" => match cmd_board(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("board: {e}");
                ExitCode::FAILURE
            }
        },
        "status" => match cmd_status(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("status: {e}");
                ExitCode::FAILURE
            }
        },
        "log" => match cmd_log(rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("log: {e}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("unknown command {other:?}");
            eprintln!("commands: init, seed, run, board, status, log");
            ExitCode::FAILURE
        }
    }
}

/// Run a future to completion on a fresh runtime.
///
/// Only the commands that touch git or the network need this; `board`/`status`
/// are synchronous store reads.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}

fn db_path(dir: &str) -> PathBuf {
    PathBuf::from(dir).join(SWARM_DIR).join("swarm.db")
}

fn open_store(dir: &str) -> Result<swarm_store::Store, String> {
    let p = db_path(dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    swarm_store::Store::open(p.to_string_lossy().as_ref()).map_err(|e| e.to_string())
}

/// `swarm init <dir>`: create the store and a git repo with one commit.
async fn cmd_init(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first().cloned() else {
        return Err("usage: swarm init <dir>".into());
    };
    let _ = open_store(&dir)?;

    let repo = Repo::new(&dir);
    if !Repo::is_repo(std::path::Path::new(&dir)) {
        repo.git(&["init", "-q"]).await.map_err(|e| e.to_string())?;
    }
    // HEAD must resolve before `git branch integration HEAD` and worktree add.
    if repo.head(None).await.is_err() {
        for (k, v) in [("user.email", "swarm@local"), ("user.name", "swarm")] {
            let _ = repo.git(&["config", k, v]).await;
        }
        repo.git(&["commit", "--allow-empty", "-m", "swarm: init"])
            .await
            .map_err(|e| e.to_string())?;
    }
    println!("initialized {dir}");
    Ok(())
}

/// `swarm seed <dir> --file f.json`
async fn cmd_seed(args: &[String]) -> Result<usize, String> {
    let Some(dir) = args.first().cloned() else {
        return Err("usage: swarm seed <dir> --file <seed.json>".into());
    };
    let mut file = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--file" && i + 1 < args.len() {
            file = Some(args[i + 1].clone());
            i += 1;
        }
        i += 1;
    }
    let file = file.ok_or("seed: --file required")?;

    let data = std::fs::read_to_string(&file).map_err(|e| format!("read {file}: {e}"))?;
    let items: Vec<SeedItem> =
        serde_json::from_str(&data).map_err(|e| format!("parse {file}: {e}"))?;

    let store = open_store(&dir)?;
    let mut n = 0;
    for it in items {
        store
            .create_item(swarm_core::CreateItemInput {
                peer: "seed".into(),
                title: it.title,
                detail: it.detail,
                parent_id: it.parent_id,
                kind: if it.kind.is_empty() {
                    "task".into()
                } else {
                    it.kind
                },
                priority: it.priority,
            })
            .map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

#[derive(serde::Deserialize)]
struct SeedItem {
    title: String,
    #[serde(default)]
    detail: String,
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    priority: i64,
}

/// `swarm run <dir> [flags]`
fn cmd_run(args: &[String]) -> ExitCode {
    let parsed: swarm_config::RunArgs = match parse_run_args(args) {
        Ok(p) => p,
        Err(ParseError::LooksLikeFlag(d)) => {
            eprintln!("run: {d:?} looks like a flag, not a directory.");
            eprintln!("usage: swarm run <dir> [flags]");
            eprintln!(
                "flags: --workers N --max-workers N --api-base URL --api-key K --model M --harness echo|direct"
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("run: {e}");
            return ExitCode::FAILURE;
        }
    };

    let dir = parsed.dir.clone();
    let cfg = parsed.config;
    match block_on(run_swarm(&dir, cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The run itself: start the API, recover, presplit, spawn, and drive the loops
/// until a stop condition fires.
async fn run_swarm(dir: &str, cfg: swarm_config::RunConfig) -> Result<(), String> {
    let store = open_store(dir)?;

    // Recover orphan claims from a previous crash.
    let _ = store.reap_expired();

    let repo = Repo::new(dir);
    repo.ensure_branch(&repo.integration(), "HEAD")
        .await
        .map_err(|e| e.to_string())?;

    let state = swarm_server::AppState::new(
        store.clone(),
        swarm_server::ServerConfig {
            forbid_item_creation: cfg.no_create,
            bind: "127.0.0.1".into(),
            port: cfg.server_port,
        },
    );
    let app = swarm_server::router(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", cfg.server_port))
        .await
        .map_err(|e| format!("bind :{}: {}", cfg.server_port, e))?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    println!(
        "swarm run: dir={} workers={} maxWorkers={} port={} harness={} exitWhenDone={}",
        dir,
        cfg.initial_workers,
        cfg.max_workers,
        cfg.server_port,
        cfg.harness_name,
        cfg.exit_when_done
    );

    let runner = std::sync::Arc::new(swarm_cli::Runner::new(
        dir.to_string(),
        cfg,
        store,
        repo,
    ));
    runner.run().await
}

/// `swarm board <dir>`
fn cmd_board(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first().cloned() else {
        return Err("usage: swarm board <dir>".into());
    };
    let store = open_store(&dir)?;
    let (items, total) = store.list_items("", 100, 0).map_err(|e| e.to_string())?;
    println!("board: {total} items");
    for it in items {
        println!("  [{}] {}", it.status, it.title);
    }
    Ok(())
}

/// `swarm status <dir>`
fn cmd_status(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first().cloned() else {
        return Err("usage: swarm status <dir>".into());
    };
    let store = open_store(&dir)?;
    let d = store.digest("cli", 0).map_err(|e| e.to_string())?;
    println!(
        "status: {} total, {} pending, {} in_progress, {} done, {} failed",
        d.stats.total, d.stats.pending, d.stats.in_progress, d.stats.done, d.stats.failed
    );
    // Seed progress is the operator's real "are we done yet" number.
    if let Ok((open, total)) = store.seed_progress() {
        if total > 0 {
            println!(
                "seed work: {}/{} terminal ({} still open)",
                total - open,
                total,
                open
            );
        }
    }
    Ok(())
}

/// `swarm log <dir> [--kind K] [--item I] [--limit N]`
fn cmd_log(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first().cloned() else {
        return Err("usage: swarm log <dir> [--kind KIND] [--item ID] [--limit N]".into());
    };
    let mut limit: i64 = 50;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" if i + 1 < args.len() => {
                limit = args[i + 1].parse().unwrap_or(50);
                i += 1;
            }
            "--kind" | "--item" => {
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let store = open_store(&dir)?;
    let entries = store.search_context("", limit).map_err(|e| e.to_string())?;
    for e in entries {
        println!("[{}] {} {} — {}", e.ts, e.kind, e.peer, e.text);
    }
    Ok(())
}
