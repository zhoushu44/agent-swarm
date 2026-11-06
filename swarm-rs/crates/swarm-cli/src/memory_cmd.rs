/// `swarm memory [list|add|search|forget|prune|compact] ...`
///
/// The cross-run memory file, independent of any project board. Kept deliberately
/// small: this is the operator's window into notes that survive between runs, not
/// a general database shell.
///
///   swarm memory                       list recent notes
///   swarm memory add <scope> <text>    remember something
///   swarm memory search <query>        substring search
///   swarm memory forget <id>           delete one
///   swarm memory prune [--days N]      drop never-served notes older than N days
///   swarm memory compact ...           fold many notes into few via the model
pub fn cmd_memory(args: &[String]) -> Result<(), String> {
    use swarm_store::MemoryStore;

    let path = MemoryStore::default_path();
    let store = MemoryStore::open(&path.to_string_lossy()).map_err(|e| e.to_string())?;

    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" | "" => {
            let mut limit: i64 = 40;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--limit" && i + 1 < args.len() {
                    limit = args[i + 1].parse().unwrap_or(40);
                    i += 1;
                }
                i += 1;
            }
            let all = store.all(limit).map_err(|e| e.to_string())?;
            println!(
                "memory: {} ({} note(s))",
                path.display(),
                store.len().map_err(|e| e.to_string())?
            );
            for m in all {
                println!(
                    "  #{:<4} [{:<6}] {:<24} {}",
                    m.id,
                    m.kind,
                    truncate(&m.scope, 24),
                    truncate(&m.text, 100)
                );
            }
            Ok(())
        }
        "add" => {
            let scope = args.get(1).ok_or("usage: swarm memory add <scope> <text>")?;
            let text = args[2..].join(" ");
            if text.trim().is_empty() {
                return Err("usage: swarm memory add <scope> <text>".into());
            }
            // Normalize a `project:` scope through the same canonicalization the
            // runner uses, so a note written from the shell lands in exactly the
            // scope a run will read. Without this, `project:C:/PROGRA~1/foo` and
            // `project:C:/Program Files/foo` are two unrelated memories.
            let scope = normalize_scope(scope);
            let id = store
                .remember(&scope, "FACT", &text, &[], "operator")
                .map_err(|e| e.to_string())?;
            println!("remembered #{id} in {scope}");
            Ok(())
        }
        "search" => {
            let q = args.get(1).ok_or("usage: swarm memory search <query>")?;
            let hits = store.search(q, 50).map_err(|e| e.to_string())?;
            if hits.is_empty() {
                println!("no notes matching {q:?}");
                return Ok(());
            }
            for m in hits {
                println!(
                    "  #{:<4} [{:<6}] {:<24} {}",
                    m.id,
                    m.kind,
                    truncate(&m.scope, 24),
                    truncate(&m.text, 100)
                );
            }
            Ok(())
        }
        "forget" => {
            let id: i64 = args
                .get(1)
                .ok_or("usage: swarm memory forget <id>")?
                .parse()
                .map_err(|_| "id must be a number")?;
            if store.forget(id).map_err(|e| e.to_string())? {
                println!("forgot #{id}");
            } else {
                println!("no note #{id}");
            }
            Ok(())
        }
        "prune" => {
            let mut days: i64 = 30;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--days" && i + 1 < args.len() {
                    days = args[i + 1].parse().unwrap_or(30);
                    i += 1;
                }
                i += 1;
            }
            let dropped = store
                .prune(days * 24 * 3600 * 1000)
                .map_err(|e| e.to_string())?;
            println!("pruned {dropped} never-served note(s) older than {days}d");
            Ok(())
        }
        "compact" => {
            let mut api_base = String::new();
            let mut api_key = String::new();
            let mut model = String::new();
            let mut dry = false;
            let mut scope: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--api-base" if i + 1 < args.len() => {
                        api_base = args[i + 1].clone();
                        i += 1;
                    }
                    "--api-key-file" if i + 1 < args.len() => {
                        api_key = std::fs::read_to_string(&args[i + 1])
                            .map_err(|e| format!("read key file {}: {e}", args[i + 1]))?
                            .trim()
                            .to_string();
                        i += 1;
                    }
                    "--api-key" if i + 1 < args.len() => {
                        api_key = args[i + 1].clone();
                        i += 1;
                    }
                    "--model" if i + 1 < args.len() => {
                        model = args[i + 1].clone();
                        i += 1;
                    }
                    "--scope" if i + 1 < args.len() => {
                        scope = Some(normalize_scope(&args[i + 1]));
                        i += 1;
                    }
                    "--dry-run" => dry = true,
                    _ => {}
                }
                i += 1;
            }
            if api_base.is_empty() || api_key.is_empty() || model.is_empty() {
                return Err("usage: swarm memory compact --api-base URL --api-key-file PATH --model M [--scope S] [--dry-run]".into());
            }

            let notes = match &scope {
                Some(s) => store.recall(&[s.clone()], 500).map_err(|e| e.to_string())?,
                None => store.all(500).map_err(|e| e.to_string())?,
            };
            if notes.len() < 2 {
                println!("nothing to compact ({} note(s))", notes.len());
                return Ok(());
            }
            println!("compacting {} note(s)...", notes.len());
            let (plan, _raw) = swarm_cli::plan_compaction(&notes, &api_base, &api_key, &model)
                .map_err(|e| format!("compact: {e}"))?;

            let covered: usize = plan.merged.iter().map(|(_, ids)| ids.len()).sum();
            println!(
                "\nplan: {} merged note(s) from {} input(s), {} dropped, {} left as-is",
                plan.merged.len(),
                covered,
                plan.drop.len(),
                notes.len().saturating_sub(covered + plan.drop.len())
            );
            for (text, ids) in &plan.merged {
                let refs = ids
                    .iter()
                    .map(|i| format!("#{i}"))
                    .collect::<Vec<_>>()
                    .join(",");
                println!("  + [{}] {}", refs, truncate(text, 110));
            }
            if !plan.drop.is_empty() {
                println!(
                    "  - drop: {}",
                    plan.drop
                        .iter()
                        .map(|i| format!("#{i}"))
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }

            if dry {
                println!("\n(--dry-run: nothing written)");
                return Ok(());
            }
            let (ins, del) = swarm_cli::apply_plan(&store, scope.as_deref(), &notes, &plan)
                .map_err(|e| format!("compact apply: {e}"))?;
            println!("\napplied: {ins} merged note(s) written, {del} old note(s) removed");
            Ok(())
        }
        other => Err(format!(
            "unknown memory subcommand {other:?}; try list|add|search|forget|prune|compact"
        )),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let mut out: String = chars[..n.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// Canonicalize a `project:` memory scope.
///
/// The runner derives its scope from `std::fs::canonicalize`, which always yields
/// the long, absolute, forward-slash form. An operator typing a path into
/// `swarm memory add` will produce something that *looks* the same but is not:
/// `C:/PROGRA~1/foo` (8.3 short name), `./foo` (relative), backslashes, a trailing
/// slash. All of those would create notes no run ever reads — and it would look
/// like it worked.
///
/// Non-`project:` scopes (`global`, `tag:x`) are returned untouched: labels, not
/// paths.
fn normalize_scope(scope: &str) -> String {
    let Some(rest) = scope.strip_prefix("project:") else {
        return scope.to_string();
    };
    // Must match `runner::memory_scopes` byte for byte, including the verbatim
    // prefix strip: Windows `canonicalize` returns `\\?\E:\path`, and storing that
    // would create a second, unreadable scope for the same directory.
    let resolved = std::fs::canonicalize(rest)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| rest.to_string());
    let cleaned = resolved
        .strip_prefix(r"\\?\")
        .or_else(|| resolved.strip_prefix("//?/"))
        .unwrap_or(&resolved)
        .replace('\\', "/");
    format!("project:{cleaned}")
}

