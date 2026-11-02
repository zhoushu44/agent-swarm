//! Read-only inspector for a `swarm.db`, used by the M4 benchmark to pull the
//! numbers the report does not print (per-guard counts, resource-lease
//! contention, the run log).
//!
//! Deliberately a throwaway binary rather than a test: it exists to answer
//! "what actually happened during that run" against a database on disk.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

fn main() {
    let db = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".swarm/swarm.db".to_string());
    let which = std::env::args().nth(2).unwrap_or_else(|| "all".to_string());

    // Read-only, and never create: opening a missing database with write access
    // would silently make an empty one and report "0 of everything".
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY;
    let conn = match Connection::open_with_flags(&db, flags) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot open {db}: {e}");
            std::process::exit(2);
        }
    };

    if which == "all" || which == "items" {
        println!("== items by status ==");
        let mut stmt = conn
            .prepare("SELECT status, count(*) FROM items GROUP BY status ORDER BY status")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        for r in rows.flatten() {
            println!("  {:<12} {}", r.0, r.1);
        }
    }

    if which == "all" || which == "subs" {
        println!("== submissions ==");
        let mut stmt = conn
            .prepare("SELECT state, count(*) FROM submissions GROUP BY state")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        for r in rows.flatten() {
            println!("  {:<10} {}", r.0, r.1);
        }
    }

    if which == "all" || which == "log" {
        println!("== run log ==");
        match conn.prepare(
            "SELECT id, kind, actor, COALESCE(item_id,''), accepted, substr(COALESCE(detail,''),1,70)
             FROM run_log ORDER BY id",
        ) {
            Ok(mut stmt) => {
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    })
                    .unwrap();
                let mut n = 0;
                for r in rows.flatten() {
                    n += 1;
                    println!(
                        "  #{} {:<12} {:<6} {:<10} accepted={} {}",
                        r.0, r.1, r.2, r.3, r.4, r.5
                    );
                }
                if n == 0 {
                    println!("  (empty)");
                }
            }
            Err(e) => println!("  (no run_log table: {e})"),
        }
    }

    if which == "all" || which == "leases" {
        println!("== resource leases ==");
        match conn.prepare("SELECT resource, peer, expires_at FROM resource_leases") {
            Ok(mut stmt) => {
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })
                    .unwrap();
                let mut n = 0;
                for r in rows.flatten() {
                    n += 1;
                    println!("  {:<28} {:<6} exp={}", r.0, r.1, r.2);
                }
                if n == 0 {
                    println!("  (none live)");
                }
            }
            Err(e) => println!("  (no resource_leases table: {e})"),
        }
    }

    if which == "all" || which == "events" {
        println!("== recent events ==");
        let mut stmt = conn
            .prepare("SELECT peer, kind, substr(text,1,100) FROM events ORDER BY id DESC LIMIT 20")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        for r in rows.flatten() {
            println!("  [{}] {} {}", r.0, r.1, r.2);
        }
    }

    if which == "all" || which == "conflicts" {
        // How often a claim was refused because of a resource lease. The events
        // table carries the reason text, so this is the contention signal.
        println!("== resource-lease refusals ==");
        let mut stmt = conn
            .prepare(
                "SELECT count(*) FROM events WHERE text LIKE '%needs resources held by another peer%'",
            )
            .unwrap();
        let n: i64 = stmt.query_row([], |r| r.get(0)).unwrap();
        println!("  {n}");
    }
}
