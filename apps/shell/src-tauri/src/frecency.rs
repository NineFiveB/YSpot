//! Per-user frecency store (SPEC.md §7.1, §7.3, §3.4 "no frecency in the
//! service").
//!
//! SQLite at `%LOCALAPPDATA%\YSpot\frecency.db`, one row per stable result id
//! (apps: AUMID; files: `volumeIdx:frn`; later: command ids). The whole table
//! is loaded into memory at startup and every launch updates memory first,
//! so ranking on the keystroke path is a hash lookup and never touches the
//! database; writes go through a channel to a thread that owns the
//! connection.
//!
//! Ranking rule (§7.1 "exact-prefix name matches MUST outrank frecency"):
//! the bonus is bounded below the 0.1 gap between match tiers, so frecency
//! reorders rows *within* a tier and can never lift a substring match over
//! a prefix match. `f = count · 0.5^(days since last launch / 7)` — a
//! half-life of a week — and `bonus = 0.09 · f / (f + 2)`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

const HALF_LIFE_DAYS: f64 = 7.0;
/// Strictly below the 0.1 tier gap of §3.4's score scale.
pub const MAX_BONUS: f32 = 0.09;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stat {
    count: u32,
    last_ts: i64,
}

struct Launch {
    id: String,
    kind: String,
    ts: i64,
}

pub struct Frecency {
    mem: RwLock<HashMap<String, Stat>>,
    /// `None` when the database could not be opened: memory-only for the
    /// session, ranking still works, nothing persists.
    tx: Option<Sender<Launch>>,
}

impl Frecency {
    /// The per-user store at its §7.1 location, or memory-only if the
    /// directory or database cannot be opened (logged, never fatal).
    pub fn open() -> Arc<Frecency> {
        match default_path() {
            Some(p) => Self::open_at(&p),
            None => {
                log::error!("frecency: LOCALAPPDATA unset; memory-only for this session");
                Arc::new(Frecency {
                    mem: RwLock::new(HashMap::new()),
                    tx: None,
                })
            }
        }
    }

    pub fn open_at(path: &Path) -> Arc<Frecency> {
        let (mem, tx) = match open_db(path) {
            Ok((conn, rows)) => {
                let (tx, rx) = mpsc::channel::<Launch>();
                let spawned = std::thread::Builder::new()
                    .name("frecency-writer".into())
                    .spawn(move || writer_loop(conn, rx));
                match spawned {
                    Ok(_) => (rows, Some(tx)),
                    Err(e) => {
                        log::error!("frecency writer thread failed to spawn: {e}");
                        (rows, None)
                    }
                }
            }
            Err(e) => {
                log::error!(
                    "frecency: cannot open {} ({e}); memory-only for this session",
                    path.display()
                );
                (HashMap::new(), None)
            }
        };
        Arc::new(Frecency {
            mem: RwLock::new(mem),
            tx,
        })
    }

    /// Record a launch now.
    pub fn record(&self, id: &str, kind: &str) {
        self.record_at(id, kind, now_ts());
    }

    fn record_at(&self, id: &str, kind: &str, ts: i64) {
        {
            let mut m = self.mem.write().unwrap_or_else(|e| e.into_inner());
            let s = m.entry(id.to_string()).or_insert(Stat {
                count: 0,
                last_ts: ts,
            });
            s.count = s.count.saturating_add(1);
            s.last_ts = ts;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(Launch {
                id: id.to_string(),
                kind: kind.to_string(),
                ts,
            });
        }
    }

    /// Ranking bonus for `id` as of now: 0 for something never launched,
    /// approaching [`MAX_BONUS`] for something launched often and recently.
    pub fn bonus(&self, id: &str) -> f32 {
        self.bonus_at(id, now_ts())
    }

    fn bonus_at(&self, id: &str, now: i64) -> f32 {
        let m = self.mem.read().unwrap_or_else(|e| e.into_inner());
        match m.get(id) {
            None => 0.0,
            Some(s) => {
                let days = (now - s.last_ts).max(0) as f64 / 86_400.0;
                let f = s.count as f64 * 0.5f64.powf(days / HALF_LIFE_DAYS);
                (MAX_BONUS as f64 * f / (f + 2.0)) as f32
            }
        }
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("YSpot").join("frecency.db"))
}

fn open_db(path: &Path) -> rusqlite::Result<(rusqlite::Connection, HashMap<String, Stat>)> {
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("frecency: create_dir_all {}: {e}", dir.display());
        }
    }
    let conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS launches (
             id      TEXT PRIMARY KEY NOT NULL,
             kind    TEXT NOT NULL,
             count   INTEGER NOT NULL,
             last_ts INTEGER NOT NULL
         );",
    )?;
    let mut rows = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, count, last_ts FROM launches")?;
        let iter = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Stat {
                    count: r.get::<_, i64>(1)?.clamp(0, u32::MAX as i64) as u32,
                    last_ts: r.get(2)?,
                },
            ))
        })?;
        for row in iter {
            let (id, stat) = row?;
            rows.insert(id, stat);
        }
    }
    log::info!(
        "frecency: {} rows loaded from {}",
        rows.len(),
        path.display()
    );
    Ok((conn, rows))
}

fn writer_loop(conn: rusqlite::Connection, rx: mpsc::Receiver<Launch>) {
    for l in rx {
        let r = conn.execute(
            "INSERT INTO launches (id, kind, count, last_ts) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 count = count + 1,
                 last_ts = excluded.last_ts,
                 kind = excluded.kind",
            rusqlite::params![l.id, l.kind, l.ts],
        );
        if let Err(e) = r {
            log::warn!("frecency: write for {} failed: {e}", l.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bonus_rewards_count_and_decays_with_age_but_stays_under_the_tier_gap() {
        let f = Frecency {
            mem: RwLock::new(HashMap::new()),
            tx: None,
        };
        let now = 1_700_000_000;
        assert_eq!(f.bonus_at("never", now), 0.0);
        f.record_at("a", "app", now);
        let one = f.bonus_at("a", now);
        assert!(one > 0.0 && one < MAX_BONUS);
        for _ in 0..50 {
            f.record_at("a", "app", now);
        }
        let many = f.bonus_at("a", now);
        assert!(many > one && many < MAX_BONUS, "{many}");
        // A week later the weight has halved.
        let later = f.bonus_at("a", now + 7 * 86_400);
        assert!(later < many);
        // Decades later it is gone, but never negative.
        let gone = f.bonus_at("a", now + 30 * 365 * 86_400);
        assert!((0.0..0.001).contains(&gone));
        // A prefix match (0.9) always beats a frecent substring match (0.55).
        assert!(0.55 + many < 0.9);
    }

    #[test]
    fn launches_persist_across_reopen() {
        let dir = std::env::temp_dir().join(format!("yspot-frecency-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("frecency.db");
        {
            let f = Frecency::open_at(&path);
            assert!(f.tx.is_some(), "database should have opened");
            f.record("app:one", "app");
            f.record("app:one", "app");
            f.record("0:42", "file");
            // Drop the sender by dropping the store; the writer thread drains
            // and exits, but give it a moment to land the rows.
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let f = Frecency::open_at(&path);
        let m = f.mem.read().unwrap();
        assert_eq!(m.get("app:one").map(|s| s.count), Some(2));
        assert_eq!(m.get("0:42").map(|s| s.count), Some(1));
        drop(m);
        drop(f);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
