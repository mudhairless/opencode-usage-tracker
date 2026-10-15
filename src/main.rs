//! opencode-usage-tracker — a KDE system tray "traffic light" for OpenCode Zen
//! free-tier usage.
//!
//! Every 30 seconds it counts today's free Zen requests in OpenCode's local
//! database (~/.local/share/opencode/opencode.db, v2 `session_message` table)
//! and colours the tray icon by the current UTC day — the day the Zen limiter
//! gates on (it resets at 00:00 UTC):
//!
//!   green  < 400 requests
//!   yellow 400–499
//!   red    >= 500
//!
//! Counting mirrors the JS loader it is based on: `assistant` rows (plus
//! `compaction` rows, which are real summarization calls) whose model is a
//! free Zen model on provider "opencode"; assistant rows that never finished
//! are skipped.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, Status, ToolTip, Tray, TrayMethods};
use rusqlite::{Connection, OpenFlags};

const POLL: Duration = Duration::from_secs(30);
const YELLOW_AT: u32 = 400;
const RED_AT: u32 = 500;
const ICON_SIZE: i32 = 22;
const AN_HOUR_IN_MS: i64 = 3_600_000;

// ---------------------------------------------------------------------------
// Counting
// ---------------------------------------------------------------------------

fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("OPENCODE_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/opencode/opencode.db")
}

/// Is an OpenCode process alive? Scans /proc and matches a process whose
/// argv[0] basename or kernel comm is exactly "opencode". Exact matching
/// keeps this tracker itself ("opencode-usage-tracker") from counting, and
/// matching argv[0] as well as comm covers both the native binary and
/// launcher shims. (Linux-only, which is fine: this is a KDE tray app.)
fn opencode_running() -> bool {
    let own_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            let args: Vec<&[u8]> = cmdline
                .split(|b| *b == 0)
                .filter(|arg| !arg.is_empty())
                .collect();
        
            if let Some(argv0) = args.first() {
                let base = argv0.rsplit(|b| *b == b'/').next().unwrap_or(argv0);
        
                if base == b"opencode" {
                    let is_service = args.get(1).copied() == Some(b"serve".as_slice())
                        && args.get(2).copied() == Some(b"--service".as_slice());
                    
        
                    if !is_service {
                        return true;
                    }
                }
            }
        }
        
    }
    false
}

/// The Zen limiter's day is the UTC day; the Unix epoch starts at a UTC
/// midnight, so the current day's start is a simple floor division.
fn utc_day_start_ms(now_ms: i64) -> i64 {
    now_ms.div_euclid(86_400_000) * 86_400_000
}

/// When the current UTC day ends — the next 00:00 UTC — expressed in the
/// machine's local time, e.g. "7:00 PM" under CDT, "6:00 PM" under CST.
/// Computed from the OS timezone via chrono's Local, so DST takes care of
/// itself.
fn reset_local_time() -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let next_reset_ms = utc_day_start_ms(now_ms) + 86_400_000;
    match chrono::DateTime::from_timestamp_millis(next_reset_ms) {
        Some(utc) => utc
            .with_timezone(&chrono::Local)
            .format("%-I:%M %p")
            .to_string(),
        None => "midnight UTC".to_string(),
    }
}

/// Same free-model rule the JS tracker's catalog reduces to: Big Pickle plus
/// every model whose id carries the `-free` suffix.
fn is_free_model(id: &str) -> bool {
    id == "big-pickle" || id.ends_with("-free")
}

/// Count today's completed free Zen requests. Returns `None` when the
/// database can't be read (missing, locked solid, schema changed) so the
/// tray can show "unknown" instead of a misleading zero.
fn count_today(path: &PathBuf) -> Option<u32> {
    // Read-write (not read-only) so a pending WAL is replayed, exactly like
    // the JS loader. We only ever SELECT.
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    let _ = conn.busy_timeout(Duration::from_secs(2));

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let start = utc_day_start_ms(now_ms);

    let mut stmt = conn
        .prepare(
            "SELECT type, data FROM session_message \
             WHERE type IN ('assistant', 'compaction') AND time_created >= ?1",
        )
        .ok()?;
    let rows = stmt
        .query_map([start], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .ok()?;

    let mut count = 0u32;
    for row in rows {
        let Ok((msg_type, data)) = row else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let model = v.get("model");
        let provider = model
            .and_then(|m| m.get("providerID"))
            .and_then(|p| p.as_str());
        let id = model.and_then(|m| m.get("id")).and_then(|p| p.as_str());
        if provider != Some("opencode") {
            continue;
        }
        let Some(id) = id else { continue };
        if !is_free_model(id) {
            continue;
        }
        // Skip assistant rows that never finished (still streaming / aborted).
        // Compaction rows carry a status instead, and are counted as-is.
        if msg_type == "assistant"
            && v.pointer("/time/completed").is_none()
            && v.get("finish").is_none()
        {
            continue;
        }
        count += 1;
    }
    Some(count)
}

/// Count the last hour's completed free Zen requests. Returns `None` when the
/// database can't be read (missing, locked solid, schema changed) so the
/// tray can show "unknown" instead of a misleading zero.
fn count_last_hour(path: &PathBuf) -> Option<u32> {
    // Read-write (not read-only) so a pending WAL is replayed, exactly like
    // the JS loader. We only ever SELECT.
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    let _ = conn.busy_timeout(Duration::from_secs(2));

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let start = now_ms - AN_HOUR_IN_MS;

    let mut stmt = conn
        .prepare(
            "SELECT type, data FROM session_message \
             WHERE type IN ('assistant', 'compaction') AND time_created >= ?1",
        )
        .ok()?;
    let rows = stmt
        .query_map([start], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .ok()?;

    let mut count = 0u32;
    for row in rows {
        let Ok((msg_type, data)) = row else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let model = v.get("model");
        let provider = model
            .and_then(|m| m.get("providerID"))
            .and_then(|p| p.as_str());
        let id = model.and_then(|m| m.get("id")).and_then(|p| p.as_str());
        if provider != Some("opencode") {
            continue;
        }
        let Some(id) = id else { continue };
        if !is_free_model(id) {
            continue;
        }
        // Skip assistant rows that never finished (still streaming / aborted).
        // Compaction rows carry a status instead, and are counted as-is.
        if msg_type == "assistant"
            && v.pointer("/time/completed").is_none()
            && v.get("finish").is_none()
        {
            continue;
        }
        count += 1;
    }
    Some(count)
}

// ---------------------------------------------------------------------------
// Tray icon: the traffic light is drawn in code (ARGB32 pixmap), no image
// files needed.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Green,
    Yellow,
    Red,
    Unknown,
}

fn level_for(count: Option<u32>) -> Level {
    match count {
        None => Level::Unknown,
        Some(n) if n >= RED_AT => Level::Red,
        Some(n) if n >= YELLOW_AT => Level::Yellow,
        Some(_) => Level::Green,
    }
}

fn make_icon(level: Level) -> Icon {
    let (r, g, b) = match level {
        Level::Green => (46u8, 204u8, 113u8),
        Level::Yellow => (241, 196, 15),
        Level::Red => (231, 76, 60),
        Level::Unknown => (127, 140, 141),
    };
    let size = ICON_SIZE;
    let c = (size as f32 - 1.0) / 2.0;
    let radius = c - 0.5;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let dist = (dx * dx + dy * dy).sqrt();
            // 1px anti-aliased edge
            let alpha = if dist <= radius - 0.5 {
                255u8
            } else if dist >= radius + 0.5 {
                0
            } else {
                ((radius + 0.5 - dist) * 255.0) as u8
            };
            // gentle top-down shading so it reads as a little lamp
            let shade = if dist <= radius {
                1.0 - (dy / (radius * 2.0) + 0.5) * 0.25
            } else {
                1.0
            };
            data.extend_from_slice(&[
                alpha,
                (r as f32 * shade) as u8,
                (g as f32 * shade) as u8,
                (b as f32 * shade) as u8,
            ]);
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}

// ---------------------------------------------------------------------------
// Tray
// ---------------------------------------------------------------------------

struct UsageTray {
    path: PathBuf,
    count: Option<u32>,
    last_hour: Option<u32>,
    running: bool,
}

impl UsageTray {
    fn refresh(&mut self) {
        self.count = count_today(&self.path);
        self.last_hour = match count_last_hour(&self.path) {
            Some(n) => Some(n),
            None => Some(0),
        };
        self.running = opencode_running(); // <- new
    }
    fn level(&self) -> Level {
    	if self.running {
        	level_for(self.count)
        } else {
  			level_for(Some(0))
        }
    }
    fn summary(&self) -> String {
        let base = match self.count {
            Some(n) => format!(
                "{n} requests today\n{:?} requests last hour\nresets at {}",
                self.last_hour.unwrap(),
                reset_local_time()
            ),
            None => "usage unavailable (database unreadable)".to_string(),
        };

        if self.running {
            format!("OpenCode Zen Free Tier Usage\n{base}")
        } else {
            format!("OpenCode Zen Free Tier Usage\nOpenCode not running\n{base}")
        }
    }
}

impl Tray for UsageTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").to_string()
    }
    fn title(&self) -> String {
        "OpenCode Usage".to_string()
    }
    fn category(&self) -> Category {
        Category::SystemServices
    }
    fn status(&self) -> Status {
        // Active = shown in the tray; Passive = Plasma tucks the item into
        // the hidden/inactive area. ksni emits the NewStatus signal when
        // this changes during handle.update(), so the item moves itself.
        if self.running {
            Status::Active
        } else {
            Status::Passive
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![make_icon(self.level())]
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: match self.count {
                Some(n) => format!("OpenCode Zen Free Tier Usage\n{n} requests today\n{:?} requests last hour", self.last_hour.unwrap()),
                None => "Usage unavailable".to_string(),
            },
            description: format!("Resets at {}", reset_local_time()),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.summary(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Refresh now".to_string(),
                activate: Box::new(|tray: &mut Self| tray.refresh()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.refresh();
    }
}

// ---------------------------------------------------------------------------
// Main: register the tray, then poll every 30s.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let path = db_path();

    // One-shot mode: print today's count and exit. Handy for testing the
    // counting logic without a tray, or from scripts.
    if std::env::args().any(|a| a == "--once") {
        match count_today(&path) {
            Some(n) => println!("{n}"),
            None => {
                eprintln!("could not read {}", path.display());
                std::process::exit(1);
            }
        }
        return;
    }

    let mut tray = UsageTray {
        path: path.clone(),
        count: None,
        last_hour: None,
        running: false,
    };
    tray.refresh(); // first paint should already be right

    let handle = match tray.spawn().await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("could not register system tray item: {e}");
            std::process::exit(1);
        }
    };

    let mut tick = tokio::time::interval(POLL);
    loop {
        tick.tick().await;
        let count = count_today(&path);
        let running = opencode_running(); // <- new
        handle
            .update(|tray: &mut UsageTray| {
                tray.count = count;
                tray.running = running; // <- new
            })
            .await;
    }
}
