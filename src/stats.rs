//! `stats` subcommand — historical free-tier usage, one row per UTC day.
//!
//! Reads the same tables and applies the same free-model rules as the tray,
//! but buckets requests and tokens per UTC day (the day the Zen limiter
//! gates on). Output is either JSON, for piping into another system, or an
//! interactive terminal chart. The chart draws as many days as bars fit
//! across the terminal — eight columns per bar with a one column gap — and
//! `--days` overrides that count; when the report spans more days than fit,
//! the arrow keys slide the window a day at a time. Bars are the default:
//! `b` switches to a line, `Tab` switches between tokens and requests.
//!
//! Below the chart sits the composable query engine (Phase 4): one query —
//! range, grouping, field selection, filters — evaluated into a single
//! row structure that the chart, the JSON output, and the text table all
//! render, so the three can never disagree. The chart is the default
//! rendering; `--json` writes the report for other tools, and `--text`
//! prints the table. Rows classify three ways: `zen-free` (the shared
//! free pool), `billed` (recorded cost above zero), and `other` (local
//! providers, third-party zero-cost rows, unknown models).

use std::io;
use std::path::PathBuf;

use crate::{
    format_tokens, free_rows_between, is_free_model, now_ms, open_db, tokens_in, utc_day_start_ms,
};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::Alignment;
use ratatui::style::Style;
use ratatui::symbols;
use ratatui::widgets::{Axis, Bar, BarChart, BarGroup, Chart, Dataset, GraphType};

const DAY_MS: i64 = 86_400_000;
/// Columns one day's bar occupies, and the gap that separates it from the
/// next. The day count that fills the screen falls out of these two.
const BAR_WIDTH: u16 = 8;
const BAR_GAP: u16 = 1;
/// Days `--json` reports when `--days` is left off: no bars to fit.
const JSON_DAYS: u32 = 7;

/// How many days fit across `width` columns at the fixed bar width: each day
/// takes `BAR_WIDTH` plus the gap after it, except the last bar's gap.
fn fit_days(width: u16) -> u32 {
    fit_count(width, BAR_WIDTH)
}

/// How many bars of `bar_width` fit across `width` columns, gaps included.
fn fit_count(width: u16, bar_width: u16) -> u32 {
    let per_day = bar_width + BAR_GAP;
    (width.saturating_add(BAR_GAP) / per_day).max(1).into()
}

/// One UTC day of free-tier usage.
#[derive(Debug)]
pub struct DayStats {
    /// UTC calendar day, e.g. "2026-10-18".
    pub date: String,
    /// Completed free-model requests (assistant + compaction rows).
    pub requests: u32,
    /// Every token bucket summed: input, output, reasoning, cache read/write.
    pub tokens: u64,
}

fn day_label(day_start_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(day_start_ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Per-day stats for the `days` most recent UTC days, oldest first, ending
/// on today (partial). Each row counts only the day it names — the query is
/// capped at the day's end so older days don't swallow the days after them.
/// `None` when the database can't be read.
pub fn collect(path: &PathBuf, days: u32) -> Option<Vec<DayStats>> {
    let conn = open_db(path)?;
    let now = now_ms()?;

    let today = utc_day_start_ms(now);
    let mut out = Vec::with_capacity(days as usize);
    for i in (0..days).rev() {
        let start = today - i as i64 * DAY_MS;
        let rows = free_rows_between(&conn, start, start + DAY_MS)?;
        let mut stats = DayStats {
            date: day_label(start),
            requests: rows.len() as u32,
            tokens: 0,
        };
        for (_, v) in &rows {
            stats.tokens += tokens_in(v);
        }
        out.push(stats);
    }
    Some(out)
}

/// Like the tray's `format_tokens`, but two decimals, so neighbouring bars
/// still read as different sizes: "47.21M", "4.05K", "812".
fn format_precise_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.2}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.2}K", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The composable query engine: one query shape, two renderers.
// ---------------------------------------------------------------------------

/// How a row's usage classifies. `zen-free` is the shared free pool
/// (provider `opencode` plus the free-model rule); `billed` is any row
/// with recorded cost above zero; everything else — local providers,
/// third-party zero-cost rows, rows with no model at all — is `other`,
/// which must never be counted as free-pool usage or as paid spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    ZenFree,
    Billed,
    Other,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::ZenFree => "zen-free",
            Category::Billed => "billed",
            Category::Other => "other",
        }
    }
}

/// One grouping dimension of a stats query, in the order the user listed
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Day,
    Model,
}

/// Which metric columns a report carries. `tokens` in `--fields` selects
/// the total plus all five component buckets; each bucket is also
/// selectable on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    pub requests: bool,
    pub tokens: bool,
    pub input: bool,
    pub output: bool,
    pub reasoning: bool,
    pub cache_read: bool,
    pub cache_write: bool,
    pub cost: bool,
}

impl Fields {
    pub fn all() -> Self {
        Fields {
            requests: true,
            tokens: true,
            input: true,
            output: true,
            reasoning: true,
            cache_read: true,
            cache_write: true,
            cost: true,
        }
    }
}

/// A whole stats query: the range is `[from_ms, to_ms)` in epoch
/// milliseconds, grouping order is significant, and a `None` filter means
/// unfiltered (rows then carry their category).
#[derive(Clone, Debug)]
pub struct Query {
    pub from_ms: i64,
    pub to_ms: i64,
    pub by: Vec<Group>,
    pub fields: Fields,
    pub model_glob: Option<String>,
    pub category: Option<Category>,
}

/// The accumulated metrics of one row (or of the totals). Token buckets
/// are kept separate; `tokens()` is their sum, the figure the tray and
/// the chart call "tokens".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl Metrics {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }

    fn add(&mut self, other: &Metrics) {
        self.requests += other.requests;
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cost += other.cost;
    }
}

/// One row of a report: the grouping keys that were asked for (plus the
/// category, when unfiltered) and the metrics accumulated under them.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportRow {
    pub date: Option<String>,
    pub model: Option<String>,
    pub category: Option<Category>,
    pub metrics: Metrics,
}

/// A finished report: rows in display order and the totals over the
/// whole filtered set (every metric, not just the displayed fields).
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub rows: Vec<ReportRow>,
    pub totals: Metrics,
}

fn classify(v: &serde_json::Value, cost: f64) -> Category {
    if cost > 0.0 {
        return Category::Billed;
    }
    let model = v.get("model");
    let provider = model
        .and_then(|m| m.get("providerID"))
        .and_then(|p| p.as_str());
    let id = model.and_then(|m| m.get("id")).and_then(|p| p.as_str());
    if provider == Some("opencode") && id.is_some_and(is_free_model) {
        Category::ZenFree
    } else {
        Category::Other
    }
}

/// The five token buckets of one message, separately. Missing or
/// non-numeric buckets count as zero, same as the sums elsewhere.
fn token_buckets(v: &serde_json::Value) -> (u64, u64, u64, u64, u64) {
    let bucket =
        |t: &serde_json::Value, key: &str| t.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    let Some(t) = v.get("tokens") else {
        return (0, 0, 0, 0, 0);
    };
    let cache = t.get("cache");
    (
        bucket(t, "input"),
        bucket(t, "output"),
        bucket(t, "reasoning"),
        cache.map_or(0, |c| bucket(c, "read")),
        cache.map_or(0, |c| bucket(c, "write")),
    )
}

/// Wildcard match for `--model`: `*` spans any run of characters, `?`
/// covers exactly one, everything else is literal.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            retry = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            retry += 1;
            ti = retry;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Parse a `--by` value: `day`, `model`, `day,model`, or `model,day`.
pub fn parse_by(text: &str) -> Result<Vec<Group>, String> {
    let mut by = Vec::new();
    for part in text.split(',') {
        let group = match part.trim() {
            "day" => Group::Day,
            "model" => Group::Model,
            other => return Err(format!("unknown grouping '{other}' (want day or model)")),
        };
        if by.contains(&group) {
            return Err(format!("grouping '{part}' listed twice"));
        }
        by.push(group);
    }
    Ok(by)
}

/// Parse a `--fields` value against the selectable metric names.
pub fn parse_fields(text: &str) -> Result<Fields, String> {
    let mut fields = Fields::default();
    for part in text.split(',') {
        match part.trim() {
            "requests" => fields.requests = true,
            "tokens" => {
                fields.tokens = true;
                fields.input = true;
                fields.output = true;
                fields.reasoning = true;
                fields.cache_read = true;
                fields.cache_write = true;
            }
            "input" => fields.input = true,
            "output" => fields.output = true,
            "reasoning" => fields.reasoning = true,
            "cache-read" => fields.cache_read = true,
            "cache-write" => fields.cache_write = true,
            "cost" => fields.cost = true,
            other => return Err(format!("unknown field '{other}'")),
        }
    }
    Ok(fields)
}

/// Parse a `YYYY-MM-DD` UTC day to the epoch millisecond it starts at.
pub fn parse_day(text: &str) -> Result<i64, String> {
    let day = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| format!("invalid date '{text}' (want YYYY-MM-DD)"))?;
    let start = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| format!("invalid date '{text}'"))?;
    Ok(start.and_utc().timestamp_millis())
}

/// Evaluate a query against `session_message`: one pass over the rows
/// in range, each finished row classified, filtered, and accumulated
/// into its group. Rows with missing cost or tokens count those as
/// zero; rows with no model group under `(unknown)`.
pub fn collect_report(path: &PathBuf, query: &Query) -> Option<Report> {
    let conn = open_db(path)?;
    let mut stmt = conn
        .prepare(
            "SELECT type, data, time_created FROM session_message \
             WHERE type IN ('assistant', 'compaction') \
             AND time_created >= ?1 AND time_created < ?2",
        )
        .ok()?;
    let rows = stmt
        .query_map([query.from_ms, query.to_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .ok()?;

    // Group key: the grouping values in the query's own order, then the
    // category when unfiltered. Date strings sort chronologically, so
    // map iteration is already display order for day groupings.
    type Key = (Option<String>, Option<String>, Option<Category>);
    let mut grouped: std::collections::BTreeMap<Key, Metrics> = std::collections::BTreeMap::new();
    let mut totals = Metrics::default();
    for row in rows {
        let Ok((msg_type, data, created)) = row else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if msg_type == "assistant"
            && v.pointer("/time/completed").is_none()
            && v.get("finish").is_none()
        {
            continue;
        }
        let cost = v.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0);
        let category = classify(&v, cost);
        if query.category.is_some_and(|c| c != category) {
            continue;
        }
        let model = v
            .get("model")
            .and_then(|m| m.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        if let Some(glob) = &query.model_glob
            && !glob_match(glob, &model)
        {
            continue;
        }
        let (input, output, reasoning, cache_read, cache_write) = token_buckets(&v);
        let metrics = Metrics {
            requests: 1,
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cost,
        };
        totals.add(&metrics);
        let date = day_label(utc_day_start_ms(created));
        let mut slots = (None, None);
        for (i, group) in query.by.iter().enumerate() {
            let value = match group {
                Group::Day => date.clone(),
                Group::Model => model.clone(),
            };
            if i == 0 {
                slots.0 = Some(value);
            } else {
                slots.1 = Some(value);
            }
        }
        let key = (
            slots.0,
            slots.1,
            if query.category.is_none() {
                Some(category)
            } else {
                None
            },
        );
        grouped.entry(key).or_default().add(&metrics);
    }

    let mut out: Vec<ReportRow> = grouped
        .into_iter()
        .map(|((first, second, category), metrics)| {
            let mut row = ReportRow {
                date: None,
                model: None,
                category,
                metrics,
            };
            for (i, group) in query.by.iter().enumerate() {
                let value = if i == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                match group {
                    Group::Day => row.date = value,
                    Group::Model => row.model = value,
                }
            }
            row
        })
        .collect();
    // Without a day grouping there is no natural order: biggest spend
    // first, then busiest, then name. Day groupings keep the map's
    // chronological order.
    if !query.by.contains(&Group::Day) {
        out.sort_by(|a, b| {
            b.metrics
                .cost
                .partial_cmp(&a.metrics.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.metrics.requests.cmp(&a.metrics.requests))
                .then(a.model.cmp(&b.model))
        });
    }
    Some(Report { rows: out, totals })
}

fn metrics_value(m: &Metrics, fields: Option<&Fields>) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    let on = |pick: fn(&Fields) -> bool| fields.is_none_or(pick);
    if on(|f| f.requests) {
        obj.insert("requests".to_string(), m.requests.into());
    }
    if on(|f| f.tokens) {
        obj.insert("tokens".to_string(), m.tokens().into());
    }
    if on(|f| f.input) {
        obj.insert("input".to_string(), m.input.into());
    }
    if on(|f| f.output) {
        obj.insert("output".to_string(), m.output.into());
    }
    if on(|f| f.reasoning) {
        obj.insert("reasoning".to_string(), m.reasoning.into());
    }
    if on(|f| f.cache_read) {
        obj.insert("cache_read".to_string(), m.cache_read.into());
    }
    if on(|f| f.cache_write) {
        obj.insert("cache_write".to_string(), m.cache_write.into());
    }
    if on(|f| f.cost) {
        obj.insert("cost".to_string(), serde_json::json!(m.cost));
    }
    serde_json::Value::Object(obj)
}

/// The JSON shape every report is sieved through:
/// `{ "rows": [...], "totals": {...} }`. Row objects carry only the
/// selected fields; totals always carry every metric.
pub fn report_json(report: &Report, fields: &Fields) -> serde_json::Value {
    let rows = report
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            if let Some(date) = &row.date {
                obj.insert("date".to_string(), date.clone().into());
            }
            if let Some(model) = &row.model {
                obj.insert("model".to_string(), model.clone().into());
            }
            if let Some(category) = row.category {
                obj.insert("category".to_string(), category.as_str().into());
            }
            if let serde_json::Value::Object(metrics) = metrics_value(&row.metrics, Some(fields)) {
                obj.extend(metrics);
            }
            serde_json::Value::Object(obj)
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "rows": rows,
        "totals": metrics_value(&report.totals, None),
    })
}

/// The text table: the same rows as `report_json`, in columns, with a
/// totals footer. Numbers are raw (no compact suffixes) so the table
/// and the JSON agree digit for digit.
pub fn render_table(report: &Report, query: &Query) -> String {
    let fields = &query.fields;
    let mut headers: Vec<&str> = Vec::new();
    if report.rows.iter().any(|r| r.date.is_some()) {
        headers.push("date");
    }
    if report.rows.iter().any(|r| r.model.is_some()) {
        headers.push("model");
    }
    if report.rows.iter().any(|r| r.category.is_some()) {
        headers.push("category");
    }
    if fields.requests {
        headers.push("requests");
    }
    if fields.tokens {
        headers.push("tokens");
    }
    if fields.input {
        headers.push("input");
    }
    if fields.output {
        headers.push("output");
    }
    if fields.reasoning {
        headers.push("reasoning");
    }
    if fields.cache_read {
        headers.push("cache-read");
    }
    if fields.cache_write {
        headers.push("cache-write");
    }
    if fields.cost {
        headers.push("cost");
    }
    let right = |header: &str| !matches!(header, "date" | "model" | "category");
    let metric_cell = |m: &Metrics, header: &str| -> String {
        match header {
            "requests" => m.requests.to_string(),
            "tokens" => m.tokens().to_string(),
            "input" => m.input.to_string(),
            "output" => m.output.to_string(),
            "reasoning" => m.reasoning.to_string(),
            "cache-read" => m.cache_read.to_string(),
            "cache-write" => m.cache_write.to_string(),
            "cost" => format!("${:.2}", m.cost),
            _ => String::new(),
        }
    };
    let row_cells = |row: &ReportRow| -> Vec<String> {
        headers
            .iter()
            .map(|header| match *header {
                "date" => row.date.clone().unwrap_or_default(),
                "model" => row.model.clone().unwrap_or_default(),
                "category" => row
                    .category
                    .map_or_else(String::new, |c| c.as_str().to_string()),
                header => metric_cell(&row.metrics, header),
            })
            .collect()
    };
    let total_cells: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            if i == 0 {
                "total".to_string()
            } else if right(header) {
                metric_cell(&report.totals, header)
            } else {
                String::new()
            }
        })
        .collect();

    let mut body: Vec<Vec<String>> = report.rows.iter().map(row_cells).collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for cells in body.iter().chain(std::iter::once(&total_cells)) {
        for (i, cell) in cells.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let format_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                if right(headers[i]) {
                    format!("{:>width$}", cell, width = widths[i])
                } else {
                    format!("{:<width$}", cell, width = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let separator = widths
        .iter()
        .map(|w| "-".repeat(*w))
        .collect::<Vec<_>>()
        .join("  ");
    let mut out = format_row(&headers.iter().map(ToString::to_string).collect::<Vec<_>>());
    out.push('\n');
    out.push_str(&separator);
    body.drain(..).for_each(|cells| {
        out.push('\n');
        out.push_str(&format_row(&cells));
    });
    out.push('\n');
    out.push_str(&separator);
    out.push('\n');
    out.push_str(&format_row(&total_cells));
    out.push('\n');
    out
}

/// The `stats` options as the CLI hands them over, before validation.
#[derive(Clone)]
pub struct StatsOptions {
    pub days: Option<u32>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub by: Option<String>,
    pub fields: Option<String>,
    pub model: Option<String>,
    pub paid: bool,
    pub free: bool,
    pub text: bool,
    pub json: bool,
}

/// Validate the CLI options into a query. Range defaults: `--days`
/// (7 without it) ending today, or `--from`/`--to` with either end
/// defaulting from the other.
pub fn build_query(opts: &StatsOptions) -> Result<Query, String> {
    if opts.paid && opts.free {
        return Err("--paid and --free cannot be combined".to_string());
    }
    let category = if opts.paid {
        Some(Category::Billed)
    } else if opts.free {
        Some(Category::ZenFree)
    } else {
        None
    };
    let by = match &opts.by {
        Some(text) => parse_by(text)?,
        None => vec![Group::Day],
    };
    let fields = match &opts.fields {
        Some(text) => parse_fields(text)?,
        None => Fields::all(),
    };
    let (from_ms, to_ms) = if opts.from.is_some() || opts.to.is_some() {
        if opts.days.is_some() {
            return Err("--days cannot be combined with --from/--to".to_string());
        }
        let to_day = match &opts.to {
            Some(text) => parse_day(text)?,
            None => utc_day_start_ms(now_ms().ok_or_else(|| "clock unavailable".to_string())?),
        };
        let from_ms = match &opts.from {
            Some(text) => parse_day(text)?,
            None => to_day - 6 * DAY_MS,
        };
        (from_ms, to_day + DAY_MS)
    } else {
        let days = i64::from(opts.days.unwrap_or(JSON_DAYS));
        let today = utc_day_start_ms(now_ms().ok_or_else(|| "clock unavailable".to_string())?);
        (today - (days - 1) * DAY_MS, today + DAY_MS)
    };
    if from_ms >= to_ms {
        return Err("--from must not be after --to".to_string());
    }
    Ok(Query {
        from_ms,
        to_ms,
        by,
        fields,
        model_glob: opts.model.clone(),
        category,
    })
}

/// Run a composable stats query to the console: JSON with `--json`,
/// the text table with `--text`. Any other invocation renders the
/// chart instead (see `run`).
pub fn run_query(path: &PathBuf, opts: &StatsOptions) -> anyhow::Result<()> {
    let query = build_query(opts).map_err(anyhow::Error::msg)?;
    let Some(report) = collect_report(path, &query) else {
        anyhow::bail!("could not read {}", path.display());
    };
    if opts.json {
        println!("{}", report_json(&report, &query.fields));
    } else {
        print!("{}", render_table(&report, &query));
    }
    Ok(())
}

/// The interactive chart: as many days as bars fit across the terminal, or
/// `--days` of them. Arrow keys slide the window a day at a time when the
/// report holds more days than fit. `q` / Esc quits.
/// One chartable metric of a report row. The chart cycles through the
/// metrics the query's field selection carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Metric {
    Tokens,
    Requests,
    Cost,
    Input,
    Output,
    Reasoning,
    CacheRead,
    CacheWrite,
}

impl Metric {
    fn name(self) -> &'static str {
        match self {
            Metric::Tokens => "tokens",
            Metric::Requests => "requests",
            Metric::Cost => "cost",
            Metric::Input => "input",
            Metric::Output => "output",
            Metric::Reasoning => "reasoning",
            Metric::CacheRead => "cache-read",
            Metric::CacheWrite => "cache-write",
        }
    }

    fn raw(self, m: &Metrics) -> f64 {
        match self {
            Metric::Tokens => m.tokens() as f64,
            Metric::Requests => m.requests as f64,
            Metric::Cost => m.cost,
            Metric::Input => m.input as f64,
            Metric::Output => m.output as f64,
            Metric::Reasoning => m.reasoning as f64,
            Metric::CacheRead => m.cache_read as f64,
            Metric::CacheWrite => m.cache_write as f64,
        }
    }

    /// Bar heights are integers: dollars become cents, everything else
    /// is already a count.
    fn bar_value(self, m: &Metrics) -> u64 {
        match self {
            Metric::Cost => (m.cost * 100.0).round().max(0.0) as u64,
            _ => self.raw(m) as u64,
        }
    }

    /// The value as it prints on a bar or in the header.
    fn text(self, m: &Metrics) -> String {
        match self {
            Metric::Tokens => format_precise_tokens(m.tokens()),
            Metric::Cost => format!("${:.2}", m.cost),
            _ => format!("{}", self.raw(m) as u64),
        }
    }

    fn y_label(self, v: f64) -> String {
        match self {
            Metric::Tokens => format_tokens(v as u64),
            Metric::Cost => format!("${v:.2}"),
            _ => format!("{v:.0}"),
        }
    }

    /// Fill color, and the color of the count printed on the fill: the
    /// count must read against the fill, so dark fills take white text
    /// and light fills take black.
    fn styles(self) -> (Style, Style) {
        match self {
            // Calm blue rather than the glaring green.
            Metric::Tokens => (Style::default().blue(), Style::default().white()),
            Metric::Requests => (Style::default().green(), Style::default().black()),
            Metric::Cost => (Style::default().yellow(), Style::default().black()),
            _ => (Style::default().cyan(), Style::default().black()),
        }
    }
}

/// The metrics a query puts on the chart's Tab cycle. The token
/// buckets ride along with the tokens total rather than cycling
/// individually; selected on their own, they cycle one by one. Cost
/// is skipped for the free pool, whose rows are zero-cost by
/// definition — an all-zero chart would say nothing.
fn metrics_for(query: &Query) -> Vec<Metric> {
    let fields = &query.fields;
    let mut out = Vec::new();
    if fields.tokens {
        out.push(Metric::Tokens);
    }
    if fields.requests {
        out.push(Metric::Requests);
    }
    if fields.cost && query.category != Some(Category::ZenFree) {
        out.push(Metric::Cost);
    }
    if !fields.tokens {
        if fields.input {
            out.push(Metric::Input);
        }
        if fields.output {
            out.push(Metric::Output);
        }
        if fields.reasoning {
            out.push(Metric::Reasoning);
        }
        if fields.cache_read {
            out.push(Metric::CacheRead);
        }
        if fields.cache_write {
            out.push(Metric::CacheWrite);
        }
    }
    if out.is_empty() {
        out.push(Metric::Requests);
    }
    out
}

/// A row's chart label: its grouping keys in order, the day shortened
/// to month and day.
fn row_label(row: &ReportRow) -> String {
    let mut parts = Vec::new();
    if let Some(date) = &row.date {
        parts.push(date[5..].to_string());
    }
    if let Some(model) = &row.model {
        parts.push(model.clone());
    }
    if let Some(category) = row.category {
        parts.push(category.as_str().to_string());
    }
    if parts.is_empty() {
        "total".to_string()
    } else {
        parts.join(" ")
    }
}

/// The bar width a report needs: the fixed width, or wide enough for
/// the longest row label (model names are long), capped so one bar
/// cannot eat the screen.
fn bar_width_for(rows: &[ReportRow]) -> u16 {
    let longest = rows.iter().map(|r| row_label(r).len()).max().unwrap_or(0) as u16;
    longest.clamp(BAR_WIDTH, 24)
}

fn run_chart(path: &PathBuf, opts: &StatsOptions) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = chart_loop(&mut terminal, path, opts);
    ratatui::restore();
    result
}

fn chart_loop(
    terminal: &mut ratatui::DefaultTerminal,
    path: &PathBuf,
    opts: &StatsOptions,
) -> io::Result<()> {
    // With no pinned range the day count follows the terminal width, as
    // the chart has always done; --days or --from/--to pins it.
    let pinned = opts.days.is_some() || opts.from.is_some() || opts.to.is_some();
    let load = |width: u16| -> Option<(Report, Vec<Metric>)> {
        let mut o = opts.clone();
        if !pinned {
            o.days = Some(fit_days(width));
        }
        let query = build_query(&o).ok()?;
        let report = collect_report(path, &query)?;
        let metrics = metrics_for(&query);
        Some((report, metrics))
    };
    let (mut report, mut metrics) = load(screen_width(terminal))
        .ok_or_else(|| io::Error::other(format!("could not read {}", path.display())))?;
    let mut window_start = 0usize; // first visible row
    let mut metric_idx = 0usize;
    let mut bars = true; // true bar chart, false line chart
    loop {
        // More rows loaded than fit on screen: show one screenful and
        // let the arrow keys slide it.
        let bar_width = bar_width_for(&report.rows);
        let max_visible = report
            .rows
            .len()
            .min(fit_count(screen_width(terminal), bar_width) as usize);
        window_start = window_start.min(report.rows.len().saturating_sub(max_visible));
        let metric = metrics[metric_idx % metrics.len()];
        terminal.draw(|frame| {
            draw(
                frame,
                &report,
                window_start,
                max_visible,
                metric,
                bars,
                bar_width,
            )
        })?;
        match event::read()? {
            // Re-fit on resize, so the bars keep filling the terminal,
            // unless the range pinned the count.
            Event::Resize(width, _) if !pinned => {
                if let Some((fresh, fresh_metrics)) = load(width) {
                    report = fresh;
                    metrics = fresh_metrics;
                    metric_idx = metric_idx.min(metrics.len() - 1);
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Left | KeyCode::Char('h') => {
                    window_start = window_start.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    window_start =
                        (window_start + 1).min(report.rows.len().saturating_sub(max_visible));
                }
                KeyCode::Tab => metric_idx = (metric_idx + 1) % metrics.len(),
                KeyCode::Char('b') => bars = !bars,
                _ => {}
            },
            _ => {}
        }
    }
}

fn screen_width(terminal: &ratatui::DefaultTerminal) -> u16 {
    terminal.size().map_or(0, |size| size.width)
}

fn draw(
    frame: &mut Frame,
    report: &Report,
    window_start: usize,
    max_visible: usize,
    metric: Metric,
    bars: bool,
    bar_width: u16,
) {
    let visible = &report.rows[window_start..window_start + max_visible];
    let span = if visible.len() > 1 {
        format!(
            "{} … {}",
            row_label(visible.first().unwrap()),
            row_label(visible.last().unwrap())
        )
    } else {
        visible
            .first()
            .map_or_else(|| "no rows in range".to_string(), row_label)
    };
    let mut summed = Metrics::default();
    for row in visible {
        summed.add(&row.metrics);
    }

    let header = format!(
        "OpenCode usage   {span}\n\
         {} {}   ←/→ slide window   Tab metric   b bars/line   q quit",
        metric.text(&summed),
        metric.name(),
    );

    let area = frame.area();
    let chunks = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Length(3),
        ratatui::layout::Constraint::Min(10),
    ])
    .split(area);

    frame.render_widget(
        ratatui::widgets::Paragraph::new(header)
            .alignment(Alignment::Center)
            .style(Style::default().cyan()),
        chunks[0],
    );

    if visible.is_empty() {
        return;
    }

    let labels: Vec<String> = visible.iter().map(row_label).collect();
    // The bar chart has no dataset names of its own, so both chart styles
    // pair a fill color with the name to label it with.
    let (color, value_style) = metric.styles();
    let name = metric.name();

    if bars {
        let columns: Vec<Bar> = visible
            .iter()
            .map(|r| {
                Bar::new(metric.bar_value(&r.metrics))
                    .label(row_label(r))
                    .text_value(metric.text(&r.metrics))
                    .style(color)
            })
            .collect();
        render_bars(frame, chunks[1], &columns, value_style, bar_width);
        // Same corner, same box as the line chart's legend.
        render_bar_legend(frame, chunks[1], name, color);
        return;
    }

    let values: Vec<f64> = visible.iter().map(|r| metric.raw(&r.metrics)).collect();
    let points: Vec<(f64, f64)> = values
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();
    let max = values
        .iter()
        .copied()
        .reduce(f64::max)
        .unwrap_or(1.0)
        .max(1.0);
    let min = values.iter().copied().reduce(f64::min).unwrap_or(0.0);
    let y0 = if min == 0.0 { 0.0 } else { min * 0.85 };
    let y1 = max * 1.15;
    let dataset = Dataset::default()
        .name(name)
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(color)
        .data(&points);
    frame.render_widget(
        Chart::new(vec![dataset])
            .x_axis(
                Axis::default()
                    .labels(labels.iter().map(|l| l.as_str()))
                    .labels_alignment(Alignment::Center)
                    .style(Style::default().dark_gray())
                    .bounds([0.0, (values.len() - 1).max(1) as f64]),
            )
            .y_axis(
                Axis::default()
                    .style(Style::default().dark_gray())
                    .labels(vec![metric.y_label(y0), metric.y_label(y1)])
                    .bounds([y0, y1]),
            ),
        chunks[1],
    );
}

/// The bar chart: the given bars at `bar_width`, separated by a single
/// empty column.
fn render_bars(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    columns: &[Bar<'_>],
    value_style: Style,
    bar_width: u16,
) {
    frame.render_widget(
        BarChart::default()
            .data(BarGroup::default().bars(columns))
            .bar_width(bar_width.min(area.width))
            .bar_gap(BAR_GAP)
            .value_style(value_style)
            .label_style(Style::default().dark_gray()),
        area,
    );
}

/// The line chart labels itself through its dataset names — a box whose border
/// sits in the terminal's own color and whose text takes the dataset's. The
/// bar chart gets the same box, in the same top-right corner. Both styles are
/// spelled out rather than left to inherit: the box overlaps the bars, and
/// inherited styles would let the fill color bleed into the border and text
/// where they cross a bar while the rest stayed in the default color.
fn render_bar_legend(frame: &mut Frame, area: ratatui::layout::Rect, name: &str, color: Style) {
    let width = (name.len() as u16 + 2).min(area.width);
    let rect = ratatui::layout::Rect {
        x: area.x + area.width - width,
        y: area.y,
        width,
        height: 3.min(area.height),
    };
    frame.render_widget(
        ratatui::widgets::Paragraph::new(name).style(color).block(
            ratatui::widgets::Block::bordered()
                .border_style(Style::default().fg(ratatui::style::Color::Reset)),
        ),
        rect,
    );
}
pub fn run(path: &PathBuf, opts: &StatsOptions) -> anyhow::Result<()> {
    // Every query renders as the chart; --json and --text go through
    // run_query instead and never reach here.
    run_chart(path, opts)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    /// A scratch database with OpenCode's `session_message` shape, so `collect`
    /// can be fed known rows without touching the real one.
    fn scratch_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "opencode-usage-tracker-{}-{name}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).expect("scratch db");
        conn.execute_batch(
            "CREATE TABLE session_message \
             (type TEXT, data TEXT, time_created INTEGER)",
        )
        .expect("schema");
        path
    }

    fn insert(conn: &Connection, at_ms: i64, model: &str, input: u64) {
        let data = serde_json::json!({
            "model": { "providerID": "opencode", "id": model },
            "tokens": { "input": input },
            "time": { "completed": 1 },
        })
        .to_string();
        conn.execute(
            "INSERT INTO session_message (type, data, time_created) VALUES ('assistant', ?1, ?2)",
            params![data, at_ms],
        )
        .expect("insert");
    }

    fn insert_row(
        conn: &Connection,
        at_ms: i64,
        model: Option<(&str, &str)>,
        input: u64,
        cost: Option<f64>,
        finished: bool,
    ) {
        let mut data = serde_json::json!({
            "tokens": { "input": input, "output": 0, "reasoning": 0,
                        "cache": { "read": 0, "write": 0 } },
            "time": { "created": at_ms },
        });
        if let Some((provider, id)) = model {
            data["model"] = serde_json::json!({ "providerID": provider, "id": id });
        }
        if let Some(c) = cost {
            data["cost"] = serde_json::json!(c);
        }
        if finished {
            data["time"]["completed"] = serde_json::json!(1);
        }
        conn.execute(
            "INSERT INTO session_message (type, data, time_created) VALUES ('assistant', ?1, ?2)",
            params![data.to_string(), at_ms],
        )
        .expect("insert");
    }

    /// Two days of mixed usage: free-pool rows, billed rows on two
    /// providers, a local row, a model-less row, and an unfinished row
    /// that must never be counted.
    fn fixture_db(name: &str) -> (PathBuf, i64) {
        let path = scratch_db(name);
        let conn = Connection::open(&path).unwrap();
        let day = parse_day("2026-10-20").unwrap();
        let hour = crate::AN_HOUR_IN_MS;
        insert_row(
            &conn,
            day + hour,
            Some(("opencode", "big-pickle")),
            1000,
            None,
            true,
        );
        insert_row(
            &conn,
            day + 2 * hour,
            Some(("opencode", "big-pickle")),
            2000,
            None,
            true,
        );
        insert_row(
            &conn,
            day + hour,
            Some(("anthropic", "claude-sonnet")),
            4000,
            Some(1.50),
            true,
        );
        insert_row(
            &conn,
            day + hour,
            Some(("local", "llama")),
            8000,
            None,
            true,
        );
        insert_row(&conn, day + hour, None, 16000, None, true);
        insert_row(
            &conn,
            day + 3 * hour,
            Some(("opencode", "big-pickle")),
            999_999,
            None,
            false,
        );
        insert_row(
            &conn,
            day + DAY_MS + hour,
            Some(("opencode", "big-pickle")),
            500,
            None,
            true,
        );
        insert_row(
            &conn,
            day + DAY_MS + 2 * hour,
            Some(("opencode", "gpt-5")),
            3000,
            Some(0.25),
            true,
        );
        drop(conn);
        (path, day)
    }

    fn query(from_ms: i64, to_ms: i64) -> Query {
        Query {
            from_ms,
            to_ms,
            by: vec![Group::Day],
            fields: Fields::all(),
            model_glob: None,
            category: None,
        }
    }

    #[test]
    fn report_groups_and_classifies() {
        let (path, day) = fixture_db("classify");
        let report = collect_report(&path, &query(day, day + 2 * DAY_MS)).expect("report");
        let _ = std::fs::remove_file(&path);
        assert_eq!(report.totals.requests, 7);
        assert_eq!(report.totals.input, 34_500);
        assert_eq!(report.totals.cost, 1.75);
        let row = |date: &str, category: Category| {
            report
                .rows
                .iter()
                .find(|r| r.date.as_deref() == Some(date) && r.category == Some(category))
                .unwrap_or_else(|| panic!("no {category:?} row for {date}"))
        };
        let free = row("2026-10-20", Category::ZenFree);
        assert_eq!(free.metrics.requests, 2);
        assert_eq!(free.metrics.tokens(), 3000);
        let billed = row("2026-10-20", Category::Billed);
        assert_eq!(billed.metrics.requests, 1);
        assert_eq!(billed.metrics.cost, 1.50);
        // The local row and the model-less row land in `other`, never in
        // the free pool and never in billed spend.
        let other = row("2026-10-20", Category::Other);
        assert_eq!(other.metrics.requests, 2);
        assert_eq!(other.metrics.input, 24_000);
        assert_eq!(other.metrics.cost, 0.0);
        assert_eq!(row("2026-10-21", Category::ZenFree).metrics.requests, 1);
        assert_eq!(row("2026-10-21", Category::Billed).metrics.cost, 0.25);
    }

    #[test]
    fn worked_example_daily_spend() {
        // stats --by day --fields cost --days 30 --paid, in query terms.
        let (path, day) = fixture_db("spend");
        let mut q = query(day - 28 * DAY_MS, day + 2 * DAY_MS);
        q.fields = parse_fields("cost").unwrap();
        q.category = Some(Category::Billed);
        let report = collect_report(&path, &q).expect("report");
        let _ = std::fs::remove_file(&path);
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].date.as_deref(), Some("2026-10-20"));
        assert_eq!(report.rows[0].metrics.cost, 1.50);
        // Totals cover every metric even when only cost is displayed.
        assert_eq!(report.totals.cost, 1.75);
        assert_eq!(report.totals.requests, 2);
    }

    #[test]
    fn worked_example_one_model_tokens() {
        // stats --model big-pickle --fields tokens --days 7
        let (path, day) = fixture_db("model-tokens");
        let mut q = query(day, day + 2 * DAY_MS);
        q.fields = parse_fields("tokens").unwrap();
        q.model_glob = Some("big-pickle".to_string());
        let report = collect_report(&path, &q).expect("report");
        let _ = std::fs::remove_file(&path);
        assert_eq!(report.totals.requests, 3);
        assert_eq!(report.totals.tokens(), 3500);
        let json = report_json(&report, &q.fields);
        let first = &json["rows"][0];
        assert_eq!(first["tokens"], 3000);
        assert!(first.get("cost").is_none());
        assert_eq!(json["totals"]["tokens"], 3500);
    }

    #[test]
    fn worked_example_paid_by_day_and_model() {
        // stats --by day,model --paid --days 14
        let (path, day) = fixture_db("paid-by-day-model");
        let mut q = query(day, day + 2 * DAY_MS);
        q.by = parse_by("day,model").unwrap();
        q.category = Some(Category::Billed);
        let report = collect_report(&path, &q).expect("report");
        let _ = std::fs::remove_file(&path);
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].model.as_deref(), Some("claude-sonnet"));
        assert_eq!(report.rows[1].model.as_deref(), Some("gpt-5"));
        // A category filter means rows no longer carry a category.
        assert!(report.rows.iter().all(|r| r.category.is_none()));
    }

    #[test]
    fn table_and_json_render_the_same_numbers() {
        let (path, day) = fixture_db("parity");
        let q = query(day, day + 2 * DAY_MS);
        let report = collect_report(&path, &q).expect("report");
        let _ = std::fs::remove_file(&path);
        let json = report_json(&report, &q.fields);
        let table = render_table(&report, &q);
        assert_eq!(json["totals"]["requests"], 7);
        assert_eq!(json["totals"]["cost"], 1.75);
        assert!(table.contains("$1.75"), "table:\n{table}");
        assert!(table.contains("zen-free"), "table:\n{table}");
        // The totals footer carries the same request count as the JSON.
        let footer = table.lines().last().unwrap();
        assert!(footer.starts_with("total"), "footer: {footer}");
        assert!(footer.contains("7"), "footer: {footer}");
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("big-pickle", "big-pickle"));
        assert!(glob_match("kimi-*", "kimi-k2.7-code"));
        assert!(glob_match("*free", "minimax-m3.1-flash-free"));
        assert!(glob_match("gpt-?", "gpt-5"));
        assert!(!glob_match("gpt-?", "gpt-55"));
        assert!(!glob_match("kimi-*", "glm-5.2"));
        assert!(glob_match("*", "(unknown)"));
    }

    #[test]
    fn option_parsing() {
        assert_eq!(
            parse_by("day,model").unwrap(),
            vec![Group::Day, Group::Model]
        );
        assert!(parse_by("week").is_err());
        assert!(parse_by("day,day").is_err());
        let fields = parse_fields("cache-read,cost").unwrap();
        assert!(fields.cache_read && fields.cost && !fields.requests);
        let fields = parse_fields("tokens").unwrap();
        assert!(fields.tokens && fields.input && fields.cache_write);
        assert!(parse_fields("vibes").is_err());
        assert_eq!(parse_day("1970-01-02").unwrap(), DAY_MS);
        assert!(parse_day("2026-13-01").is_err());
    }

    #[test]
    fn build_query_rejects_conflicts() {
        let blank = StatsOptions {
            days: None,
            from: None,
            to: None,
            by: None,
            fields: None,
            model: None,
            paid: false,
            free: false,
            text: false,
            json: false,
        };
        let both_filters = StatsOptions {
            paid: true,
            free: true,
            ..blank
        };
        assert!(build_query(&both_filters).is_err());
        let blank = StatsOptions {
            days: None,
            from: None,
            to: None,
            by: None,
            fields: None,
            model: None,
            paid: false,
            free: false,
            text: false,
            json: false,
        };
        let mixed_range = StatsOptions {
            days: Some(7),
            from: Some("2026-10-01".to_string()),
            ..blank
        };
        assert!(build_query(&mixed_range).is_err());
    }

    #[test]
    fn chart_metric_cycle_follows_fields() {
        let mut q = query(0, 0);
        assert_eq!(
            metrics_for(&q),
            vec![Metric::Tokens, Metric::Requests, Metric::Cost]
        );
        q.fields = parse_fields("cost").unwrap();
        assert_eq!(metrics_for(&q), vec![Metric::Cost]);
        q.fields = parse_fields("input").unwrap();
        assert_eq!(metrics_for(&q), vec![Metric::Input]);
        // `tokens` selected: the buckets ride along with the total
        // rather than cycling one by one.
        q.fields = parse_fields("tokens").unwrap();
        assert_eq!(metrics_for(&q), vec![Metric::Tokens]);
        // The free pool is zero-cost by definition: no cost chart.
        q.fields = Fields::all();
        q.category = Some(Category::ZenFree);
        assert_eq!(metrics_for(&q), vec![Metric::Tokens, Metric::Requests]);
    }

    #[test]
    fn chart_labels_and_cost_scaling() {
        let row = ReportRow {
            date: Some("2026-10-20".to_string()),
            model: None,
            category: Some(Category::Billed),
            metrics: Metrics {
                cost: 1.75,
                ..Metrics::default()
            },
        };
        assert_eq!(row_label(&row), "10-20 billed");
        // Bars are integer heights: dollars are charted as cents.
        assert_eq!(Metric::Cost.bar_value(&row.metrics), 175);
        assert_eq!(Metric::Cost.text(&row.metrics), "$1.75");
        // Bars widen to fit their labels, capped at 24 columns.
        assert_eq!(bar_width_for(std::slice::from_ref(&row)), 12);
        assert_eq!(fit_count(100, 17), 5);
    }

    /// `fit_days` decides how many bars fill the terminal: 8 columns each
    /// plus a one column gap, except after the last bar.
    #[test]
    fn fit_days_counts_whole_bars() {
        assert_eq!(fit_days(0), 1); // never zero days
        assert_eq!(fit_days(8), 1); // exactly one bare bar
        assert_eq!(fit_days(16), 1); // a second bar would need 17
        assert_eq!(fit_days(17), 2);
        assert_eq!(fit_days(100), 11); // a twelfth would need 107
    }

    /// Regression test for the chart drawing a straight line down to zero: every
    /// day used to be queried with "since day start" and no end, so each bucket
    /// held that day *and every day after it* — a declining running total that
    /// has nothing to do with per-day usage. Buckets must be disjoint.
    #[test]
    fn each_day_counts_only_its_own_rows() {
        let path = scratch_db("buckets");
        let conn = Connection::open(&path).unwrap();
        let today = utc_day_start_ms(now_ms().unwrap());

        // One free request per day, at noon, each with a distinct token count.
        for i in 0..7u64 {
            insert(
                &conn,
                today - i as i64 * DAY_MS + crate::AN_HOUR_IN_MS * 12,
                "big-pickle",
                1_000 * (i + 1),
            );
        }
        // A paid model and a row from tomorrow: neither may show up in any
        // bucket.
        insert(&conn, today + crate::AN_HOUR_IN_MS * 3, "gpt-5", 999_999);
        insert(
            &conn,
            today + DAY_MS + crate::AN_HOUR_IN_MS,
            "big-pickle",
            999_999,
        );
        drop(conn);

        let stats = collect(&path, 7).expect("collect");
        let _ = std::fs::remove_file(&path);

        assert_eq!(stats.len(), 7);
        // Oldest first, so the biggest day comes first: if buckets leaked
        // forward again, the oldest day would tower over the rest.
        for (i, day) in stats.iter().rev().enumerate() {
            assert_eq!(day.requests, 1, "requests on {}", day.date);
            assert_eq!(day.tokens, 1_000 * (i as u64 + 1), "tokens on {}", day.date);
        }
    }
}
