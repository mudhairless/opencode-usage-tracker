//! opencode-usage-tracker — a KDE system tray "traffic light" for OpenCode Zen
//! free-tier usage.
//!
//! Every 30 seconds it counts today's free Zen requests in OpenCode's local
//! database (~/.local/share/opencode/opencode.db, v2 `session_message` table)
//! and colours the tray icon by the current UTC day — the day the Zen limiter
//! gates on (it resets at 00:00 UTC):
//!
//!   green  < 100 requests
//!   yellow 100–149
//!   red    >= 150
//!
//! Counting mirrors the JS loader it is based on: `assistant` rows (plus
//! `compaction` rows, which are real summarization calls) whose model is a
//! free Zen model on provider "opencode"; assistant rows that never finished
//! are skipped.
//!
//! Usage:
//!   opencode-usage-tracker                    run the tray
//!   opencode-usage-tracker --once             print the tray's hover text and exit
//!   opencode-usage-tracker install            install for you (user paths)
//!   opencode-usage-tracker install --system --autostart
//!   opencode-usage-tracker uninstall          remove the install

mod install;
mod stats;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, Status, ToolTip, Tray, TrayMethods};
use rusqlite::{Connection, OpenFlags};

const SPIN_TICK: Duration = Duration::from_millis(250);
const TICKS_PER_POLL: u64 = 120;
const SPIN_FRAMES: u8 = 8;
const ARC: f32 = 2.1; // radians of ring lit behind the head (~120°)
const YELLOW_AT: u32 = 100;
const RED_AT: u32 = 150;
const ICON_SIZE: i32 = 44;
const THROBBER_THICKNESS: f32 = 2.4;
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

/// Is the newest session mid-run?
///
/// OpenCode appends a row to `session_message` as a run progresses: the `user`
/// prompt, then one or more `assistant` messages (each stamped with
/// `time.completed` once its step ends), and finally an `idle` message when
/// the run finishes. So a run is in flight exactly while the newest assistant
/// message has no `time.completed` yet.
///
/// This deliberately does not use `session_v2.time_updated` vs `time_idle`:
/// several OpenCode projectors (title generation, usage roll-ups, metadata)
/// bump `time_updated` *after* the terminal `time_idle` stamp, so
/// `time_updated > time_idle` latches true and would spin the icon forever.
///
/// One-row lookup, cheap enough to poll every second. `false` when the DB
/// can't be read or the session has no assistant message yet.
fn session_working(path: &PathBuf) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE) else {
        return false;
    };
    let _ = conn.busy_timeout(Duration::from_millis(250));
    working_from(&conn)
}

fn working_from(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT json_extract(m.data, '$.time.completed')
           FROM session_v2 s
           JOIN session_message m
             ON m.session_id = s.id
            AND m.seq = (SELECT MAX(m2.seq) FROM session_message m2
                          WHERE m2.session_id = s.id AND m2.type = 'assistant')
          WHERE s.id = (SELECT id FROM session_v2 ORDER BY time_updated DESC LIMIT 1)",
        [],
        |row| Ok(row.get::<_, Option<i64>>(0)?.is_none()),
    )
    .unwrap_or(false)
}

fn now_ms() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
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

/// Open OpenCode's database read-write (not read-only) so a pending WAL is
/// replayed, exactly like the JS loader. We only ever SELECT.
fn open_db(path: &PathBuf) -> Option<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).ok()?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    Some(conn)
}

/// Every completed free Zen message created in `[start_ms, end_ms)`, as
/// (message type, parsed data). Same free-model rule the JS tracker's catalog
/// reduces to: Big Pickle plus every model whose id carries the `-free`
/// suffix. Assistant rows that never finished (still streaming / aborted) are
/// skipped; compaction rows carry a status instead and count as-is.
fn free_rows_between(
    conn: &Connection,
    start_ms: i64,
    end_ms: i64,
) -> Option<Vec<(String, serde_json::Value)>> {
    let mut stmt = conn
        .prepare(
            "SELECT type, data FROM session_message \
             WHERE type IN ('assistant', 'compaction') \
             AND time_created >= ?1 AND time_created < ?2",
        )
        .ok()?;
    let rows = stmt
        .query_map([start_ms, end_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .ok()?;

    let mut out = Vec::new();
    for row in rows {
        let Ok((msg_type, data)) = row else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let model = v.get("model");
        let provider = model
            .and_then(|m| m.get("providerID"))
            .and_then(|p| p.as_str());
        if provider != Some("opencode") {
            continue;
        }
        let Some(id) = model.and_then(|m| m.get("id")).and_then(|p| p.as_str()) else {
            continue;
        };
        if !is_free_model(id) {
            continue;
        }
        if msg_type == "assistant"
            && v.pointer("/time/completed").is_none()
            && v.get("finish").is_none()
        {
            continue;
        }
        out.push((msg_type, v));
    }
    Some(out)
}

/// Every completed free Zen message created at or after `start_ms`: the
/// open-ended query the tray's "now" counters use (today, last hour).
fn free_rows_since(conn: &Connection, start_ms: i64) -> Option<Vec<(String, serde_json::Value)>> {
    free_rows_between(conn, start_ms, i64::MAX)
}

/// Count today's completed free Zen requests. Returns `None` when the
/// database can't be read (missing, locked solid, schema changed) so the
/// tray can show "unknown" instead of a misleading zero.
fn count_today(path: &PathBuf) -> Option<u32> {
    let conn = open_db(path)?;
    let start = utc_day_start_ms(now_ms()?);
    Some(free_rows_since(&conn, start)?.len() as u32)
}

/// Count the last hour's completed free Zen requests. Returns `None` when the
/// database can't be read (missing, locked solid, schema changed) so the
/// tray can show "unknown" instead of a misleading zero.
fn count_last_hour(path: &PathBuf) -> Option<u32> {
    let conn = open_db(path)?;
    let start = now_ms()? - AN_HOUR_IN_MS;
    Some(free_rows_since(&conn, start)?.len() as u32)
}

/// The five token buckets OpenCode stamps on an assistant message:
/// `{input, output, reasoning, cache: {read, write}}`. Anything absent or
/// non-numeric counts as zero.
fn tokens_in(v: &serde_json::Value) -> u64 {
    let t = match v.get("tokens") {
        Some(t) => t,
        None => return 0,
    };
    let bucket =
        |n: &serde_json::Value, key: &str| n.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    bucket(t, "input")
        + bucket(t, "output")
        + bucket(t, "reasoning")
        + t.get("cache").map_or(0, |c| bucket(c, "read"))
        + t.get("cache").map_or(0, |c| bucket(c, "write"))
}

/// How many tokens the free Zen models consumed during the UTC day starting
/// at `day_start_ms` (the day the Zen limiter gates on). Sums every bucket
/// OpenCode records, so cached input is included. `None` when the database
/// can't be read.
fn tokens_on_day(path: &PathBuf, day_start_ms: i64) -> Option<u64> {
    let conn = open_db(path)?;
    let rows = free_rows_since(&conn, day_start_ms)?;
    Some(rows.iter().map(|(_, v)| tokens_in(v)).sum())
}

/// Today's free-model token total, for the current UTC day.
fn tokens_today(path: &PathBuf) -> Option<u64> {
    tokens_on_day(path, utc_day_start_ms(now_ms()?))
}

/// "812", "34.5K", "1.2M" — keeps the hover tooltip to one short line.
fn format_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1_000.0),
        n => n.to_string(),
    }
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

fn make_icon(level: Level, spin: Option<u8>) -> Icon {
    let (r, g, b) = match level {
        Level::Green => (46u8, 204u8, 113u8),
        Level::Yellow => (241, 196, 15),
        Level::Red => (231, 76, 60),
        Level::Unknown => (127, 140, 141),
    };
    let size = ICON_SIZE;
    let c = (size as f32 - 1.0) / 2.0;
    let radius = if spin.is_some() { c - 3.5 } else { c - 0.5 };
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
            if let Some(frame) = spin {
                let ring_r = c - 1.25;
                if (dist - ring_r).abs() <= THROBBER_THICKNESS {
                    let phi = (-dy).atan2(dx);
                    let phi = if phi < 0.0 {
                        phi + std::f32::consts::TAU
                    } else {
                        phi
                    };
                    let head = (frame as f32 / SPIN_FRAMES as f32) * std::f32::consts::TAU;
                    let behind = (head - phi).rem_euclid(std::f32::consts::TAU);
                    if behind < ARC {
                        let fade = 1.0 - behind / ARC;
                        data.extend_from_slice(&[(fade * 255.0) as u8, 255, 255, 255]);
                        continue;
                    }
                }
            }

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

/// What hovering the tray shows — `--once` prints exactly this.
struct Hover {
    title: String,
    description: String,
}

struct UsageTray {
    path: PathBuf,
    count: Option<u32>,
    last_hour: Option<u32>,
    tokens: Option<u64>,
    recent: Option<Vec<stats::DayStats>>,
    running: bool,
    working: bool,
    frame: u8,
}

impl UsageTray {
    fn refresh(&mut self) {
        self.count = count_today(&self.path);
        self.last_hour = count_last_hour(&self.path);
        self.tokens = tokens_today(&self.path);
        self.recent = stats::collect(&self.path, 3);
        self.running = opencode_running(); // <- new
        // A run can't be in flight if OpenCode isn't running. Gating here also
        // covers a session OpenCode left unfinished (killed mid-run) instead
        // of spinning forever.
        self.working = self.running && session_working(&self.path);
    }
    fn level(&self) -> Level {
        level_for(self.count)
    }
    /// The hover text: title lines plus the reset time. Shared with `--once`
    /// so the CLI and the tray can't drift apart.
    fn hover(&self) -> Hover {
        let mut title = "OpenCode Zen Free Tier Usage".to_string();
        if !self.running {
            title.push_str("\nOpenCode not running");
        }
        match self.count {
            Some(n) => {
                title.push_str(&format!(
                    "\n{n} requests today\n{} requests last hour",
                    self.last_hour
                        .map_or_else(|| "?".to_string(), |m| m.to_string())
                ));
                match self.tokens {
                    Some(t) => title.push_str(&format!("\n{} tokens today", format_tokens(t))),
                    None => title.push_str("\ntokens unavailable"),
                }
                match &self.recent {
                    Some(days) => {
                        let (requests, tokens) = days
                            .iter()
                            .fold((0u32, 0u64), |(r, t), d| (r + d.requests, t + d.tokens));
                        title.push_str(&format!(
                            "\n{requests} requests last 3 days · {} tokens",
                            format_tokens(tokens)
                        ));
                    }
                    None => title.push_str("\nlast 3 days unavailable"),
                }
            }
            None => title.push_str("\nusage unavailable (database unreadable)"),
        }
        Hover {
            title,
            description: format!("Resets at {}", reset_local_time()),
        }
    }

    fn summary(&self) -> String {
        let base = match self.count {
            Some(n) => {
                let mut s = format!(
                    "{n} requests today\n{} requests last hour",
                    self.last_hour
                        .map_or_else(|| "?".to_string(), |m| m.to_string()),
                );
                match self.tokens {
                    Some(t) => s.push_str(&format!("\n{} tokens today", format_tokens(t))),
                    None => s.push_str("\ntokens unavailable"),
                }
                s.push_str(&format!("\nresets at {}", reset_local_time()));
                match &self.recent {
                    Some(days) => {
                        s.push_str("\nLast 3 days:");
                        let last = days.len().saturating_sub(1);
                        for (i, day) in days.iter().enumerate() {
                            let today = if i == last { " (today)" } else { "" };
                            s.push_str(&format!(
                                "\n{}: {} requests · {} tokens{today}",
                                day.date,
                                day.requests,
                                format_tokens(day.tokens)
                            ));
                        }
                    }
                    None => s.push_str("\nlast 3 days unavailable"),
                }
                s
            }
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
        // While working we must stay Active even if OpenCode has just closed,
        // otherwise Plasma stops repainting and the spin animation freezes.
        if self.running || self.working {
            Status::Active
        } else {
            Status::Passive
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let spin = if self.working { Some(self.frame) } else { None };
        vec![make_icon(self.level(), spin)]
    }

    fn tool_tip(&self) -> ToolTip {
        let hover = self.hover();
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: hover.title,
            description: hover.description,
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
// Command line
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    version,
    about = "KDE tray traffic light for OpenCode Zen free-tier usage",
    after_help = "examples:\n  opencode-usage-tracker                      run the tray\n  opencode-usage-tracker --once               print the hover text and exit\n  opencode-usage-tracker install              install for you\n  opencode-usage-tracker install --system --autostart\n  opencode-usage-tracker uninstall            remove the install"
)]
struct Cli {
    /// Print the tray's hover text once and exit
    #[arg(long)]
    once: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Copy this executable and its .desktop entry into place
    Install {
        /// Install system-wide: /usr/local/bin + /usr/share/applications
        #[arg(long)]
        system: bool,

        /// Also start the tray at login — written to the invoking user's own
        /// autostart dir (yours, even under sudo), never the global one
        #[arg(long)]
        autostart: bool,

        /// Install this file instead of the running executable
        #[arg(long)]
        from: Option<PathBuf>,
    },

    /// Remove everything `install` created
    Uninstall {
        /// Remove a system-wide install (needs root)
        #[arg(long)]
        system: bool,
    },

    /// Show historical free-tier usage per UTC day: a terminal chart, or
    /// JSON with --json. The chart fits as many days as bars fit on screen;
    /// the arrow keys slide the window when more days are loaded than fit.
    Stats {
        /// How many days back to report (1 or more). Defaults to the most
        /// days whose bars fit on screen (7 with --json)
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=365))]
        days: Option<u32>,

        /// Print JSON (per-day rows plus totals) instead of the chart
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Main: register the tray, then poll every 30s.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Some(cmd) = &cli.command {
        let outcome = match cmd {
            Command::Install {
                system,
                autostart,
                from,
            } => install::install(*system, *autostart, from.as_deref()),
            Command::Uninstall { system } => install::uninstall(*system),
            Command::Stats { days, json } => stats::run(&db_path(), *days, *json),
        };
        if let Err(e) = outcome {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let path = db_path();

    // One-shot mode: print the hover text (the same lines the tray shows) and
    // exit. Handy for testing the counting logic without a tray, or in scripts.
    if cli.once {
        let mut snapshot = UsageTray {
            path: path.clone(),
            count: None,
            last_hour: None,
            tokens: None,
            recent: None,
            running: false,
            working: false,
            frame: 0,
        };
        snapshot.refresh();
        let hover = snapshot.hover();
        println!("{}\n{}", hover.title, hover.description);
        if snapshot.count.is_none() {
            eprintln!("could not read {}", path.display());
            std::process::exit(1);
        }
        return;
    }

    let mut tray = UsageTray {
        path: path.clone(),
        count: None,
        last_hour: None,
        tokens: None,
        recent: None,
        running: false,
        working: false,
        frame: 0,
    };
    tray.refresh(); // first paint should already be right
    let mut working = tray.working;
    let handle = match tray.spawn().await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("could not register system tray item: {e}");
            std::process::exit(1);
        }
    };

    let mut beat = tokio::time::interval(SPIN_TICK);
    let mut ticks: u64 = 0;

    let mut frame: u8 = 0;
    loop {
        beat.tick().await;
        ticks = ticks.wrapping_add(1);

        let was_working = working;
        if ticks.is_multiple_of(4) {
            // 1s cadence. The /proc scan is cheap, and gating on it clears
            // `working` when OpenCode isn't running (e.g. killed mid-run).
            working = opencode_running() && session_working(&path);
        }

        if ticks.is_multiple_of(TICKS_PER_POLL) {
            // your existing full poll, unchanged apart from also storing working
            let count = count_today(&path);
            let last_hour = count_last_hour(&path);
            let tokens = tokens_today(&path);
            let recent = stats::collect(&path, 3);
            let running = opencode_running();
            let w = working;
            handle
                .update(|tray: &mut UsageTray| {
                    tray.count = count;
                    tray.last_hour = last_hour;
                    tray.tokens = tokens;
                    tray.recent = recent;
                    tray.running = running;
                    tray.working = w;
                })
                .await;
            continue;
        }

        if working {
            frame = (frame + 1) % SPIN_FRAMES;
            let f = frame;
            handle
                .update(|tray: &mut UsageTray| {
                    tray.working = true;
                    tray.frame = f;
                })
                .await;
        } else if was_working {
            // one final update so the ring freezes back to the plain disc
            handle
                .update(|tray: &mut UsageTray| tray.working = false)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tray(count: Option<u32>, running: bool) -> UsageTray {
        UsageTray {
            path: PathBuf::new(),
            count,
            last_hour: Some(3),
            tokens: Some(12345),
            recent: Some(vec![
                stats::DayStats {
                    date: "2026-10-27".to_string(),
                    requests: 5,
                    tokens: 1_000,
                },
                stats::DayStats {
                    date: "2026-10-28".to_string(),
                    requests: 6,
                    tokens: 2_000,
                },
                stats::DayStats {
                    date: "2026-10-29".to_string(),
                    requests: 7,
                    tokens: 3_000,
                },
            ]),
            running,
            working: false,
            frame: 0,
        }
    }

    #[test]
    fn hover_text_matches_the_tray() {
        let hover = tray(Some(12), true).hover();
        assert_eq!(
            hover.title,
            "OpenCode Zen Free Tier Usage\n12 requests today\n3 requests last hour\n12.3K tokens today\n18 requests last 3 days · 6.0K tokens"
        );
        assert!(hover.description.starts_with("Resets at "));
    }

    #[test]
    fn summary_lists_each_of_the_last_three_days() {
        let summary = tray(Some(12), true).summary();
        assert!(summary.contains("\nLast 3 days:"));
        assert!(summary.contains("\n2026-10-27: 5 requests · 1.0K tokens"));
        assert!(summary.contains("\n2026-10-28: 6 requests · 2.0K tokens"));
        assert!(summary.contains("\n2026-10-29: 7 requests · 3.0K tokens (today)"));
    }

    #[test]
    fn missing_recent_stats_are_reported_as_unavailable() {
        let mut t = tray(Some(12), true);
        t.recent = None;
        assert!(t.hover().title.contains("\nlast 3 days unavailable"));
        assert!(t.summary().contains("\nlast 3 days unavailable"));
    }

    #[test]
    fn hover_notes_a_stopped_opencode() {
        let hover = tray(Some(0), false).hover();
        assert!(hover.title.contains("OpenCode not running"));
    }

    #[test]
    fn hover_reports_an_unreadable_database() {
        let hover = tray(None, true).hover();
        assert!(
            hover
                .title
                .contains("usage unavailable (database unreadable)")
        );
    }

    /// Build the two tables `working_from` touches and seed one newest session.
    fn working_db(last_assistant_completed: Option<i64>) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, time_updated INTEGER NOT NULL);
             CREATE TABLE session_message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 type TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_v2 (id, time_updated) VALUES ('ses_a', 100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, data)
             VALUES ('m1', 'ses_a', 'user', 1, '{\"time\":{\"created\":1}}')",
            [],
        )
        .unwrap();
        let completed = last_assistant_completed
            .map(|t| format!(",\"completed\":{t}"))
            .unwrap_or_default();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, data)
             VALUES ('m2', 'ses_a', 'assistant', 2, ?1)",
            [format!("{{\"time\":{{\"created\":2{completed}}}}}")],
        )
        .unwrap();
        conn
    }

    #[test]
    fn working_while_the_newest_assistant_step_is_unfinished() {
        assert!(working_from(&working_db(None)));
    }

    #[test]
    fn idle_once_the_assistant_step_completes() {
        assert!(!working_from(&working_db(Some(3))));
    }
}
