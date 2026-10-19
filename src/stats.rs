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

use std::io;
use std::path::PathBuf;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::widgets::{Axis, Bar, BarChart, BarGroup, Chart, Dataset, GraphType};
use ratatui::style::Style;
use ratatui::layout::Alignment;
use ratatui::Frame;
use ratatui::symbols;
use serde::Serialize;

use crate::{free_rows_between, format_tokens, now_ms, open_db, utc_day_start_ms, tokens_in};

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
    let per_day = BAR_WIDTH + BAR_GAP;
    (width.saturating_add(BAR_GAP) / per_day).max(1).into()
}

/// One UTC day of free-tier usage.
#[derive(Debug, Serialize)]
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

fn total(stats: &[DayStats]) -> (u32, u64) {
    stats.iter().fold((0, 0), |(r, t), d| (r + d.requests, t + d.tokens))
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

fn render_json(stats: &[DayStats]) -> String {
    let (requests, tokens) = total(stats);
    serde_json::json!({
        "generated_at_utc": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "days": stats.iter().map(|d| serde_json::json!({
            "date": d.date,
            "requests": d.requests,
            "tokens": d.tokens
        })).collect::<Vec<_>>(),
        "totals": { "requests": requests, "tokens": tokens },
    }).to_string()
}

/// The interactive chart: as many days as bars fit across the terminal, or
/// `--days` of them. Arrow keys slide the window a day at a time when the
/// report holds more days than fit. `q` / Esc quits.
fn run_chart(path: &PathBuf, days: Option<u32>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = chart_loop(&mut terminal, path, days);
    ratatui::restore();
    result
}

fn chart_loop(
    terminal: &mut ratatui::DefaultTerminal,
    path: &PathBuf,
    days: Option<u32>,
) -> io::Result<()> {
    let load = |width: u16| collect(path, days.unwrap_or_else(|| fit_days(width)));
    let mut stats = load(screen_width(terminal))
        .ok_or_else(|| io::Error::other(format!("could not read {}", path.display())))?;
    let mut window_start = 0usize; // oldest visible day
    let mut mode = 0; // 0 tokens, 1 requests
    let mut bars = true; // true bar chart, false line chart
    loop {
        // More days loaded than fit on screen: show one screenful and let the
        // arrow keys slide it.
        let max_visible = stats.len().min(fit_days(screen_width(terminal)) as usize);
        window_start = window_start.min(stats.len() - max_visible);
        terminal.draw(|frame| draw(frame, &stats, window_start, max_visible, mode, bars))?;
        match event::read()? {
            // Re-fit on resize, so the bars keep filling the terminal, unless
            // --days pinned the count.
            Event::Resize(width, _) if days.is_none() => {
                if let Some(fresh) = load(width) {
                    stats = fresh;
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Left | KeyCode::Char('h') => {
                    window_start = window_start.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    window_start = (window_start + 1).min(stats.len() - max_visible);
                }
                KeyCode::Tab => mode = 1 - mode,
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
    stats: &[DayStats],
    window_start: usize,
    max_visible: usize,
    mode: usize,
    bars: bool,
) {
    let visible = &stats[window_start..window_start + max_visible];
    let (requests, tokens) = total(visible);
    let span = if visible.len() > 1 {
        format!("{} … {}", visible.first().unwrap().date, visible.last().unwrap().date)
    } else {
        visible.first().map_or_else(|| "?".to_string(), |d| d.date.clone())
    };

    let header = format!(
        "OpenCode Zen free-tier usage   {span}\n\
         {} requests   {} tokens   ←/→ slide window   Tab metric   b bars/line   q quit",
        format_tokens(requests as u64),
        format_tokens(tokens),
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

    let labels: Vec<String> = visible.iter().map(|d| d.date[5..].to_string()).collect();
    let values: Vec<u64> = visible
        .iter()
        .map(|d| if mode == 0 { d.tokens } else { d.requests as u64 })
        .collect();
    // The bar chart has no dataset names of its own, so both chart styles
    // pair a fill color with the name to label it with.
    let (color, name) = if mode == 0 {
        // Calm blue rather than the glaring green.
        (Style::default().blue(), "tokens/day")
    } else {
        (Style::default().green(), "requests/day")
    };
    // The count prints on the fill, so it must read against that color:
    // white sinks into nothing on the light cyan, black on the dark blue.
    let value_style = if mode == 0 {
        Style::default().white()
    } else {
        Style::default().black()
    };

    if bars {
        // Tokens print like "47.21M"; request counts are small enough to
        // print whole.
        let texts: Vec<String> = if mode == 0 {
            values.iter().map(|v| format_precise_tokens(*v)).collect()
        } else {
            values.iter().map(ToString::to_string).collect()
        };
        render_bars(frame, chunks[1], &values, &texts, &labels, color, value_style);
        // Same corner, same box as the line chart's legend.
        render_bar_legend(frame, chunks[1], name, color);
        return;
    }

    let points: Vec<(f64, f64)> = values
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v as f64))
        .collect();
    let max = values.iter().copied().max().unwrap_or(1).max(1) as f64;
    let min = values.iter().copied().min().unwrap_or(0) as f64;
    let y0 = if min == 0.0 { 0.0 } else { min * 0.85 };
    let y1 = max * 1.15;
    // Tokens get the tray's compact "12.3K" form; request counts are small
    // enough to print whole.
    let y_label = |v: f64| {
        if mode == 0 {
            format_tokens(v as u64)
        } else {
            format!("{v:.0}")
        }
    };
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
                    .labels(vec![y_label(y0), y_label(y1)])
                    .bounds([y0, y1]),
            ),
        chunks[1],
    );
}

/// The bar chart: one bar per day at the fixed `BAR_WIDTH`, separated by a
/// single empty column, with the day's count printed at the bar's base as
/// `texts` says it.
fn render_bars(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    values: &[u64],
    texts: &[String],
    labels: &[String],
    color: Style,
    value_style: Style,
) {
    let columns: Vec<Bar<'_>> = values
        .iter()
        .zip(texts.iter())
        .zip(labels.iter())
        .map(|((value, text), label)| {
            Bar::new(*value)
                .label(label.as_str())
                .text_value(text.clone())
                .style(color)
        })
        .collect();
    frame.render_widget(
        BarChart::default()
            .data(BarGroup::default().bars(&columns))
            .bar_width(BAR_WIDTH.min(area.width))
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
fn render_bar_legend(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    name: &str,
    color: Style,
) {
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
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    /// A scratch database with OpenCode's `session_message` shape, so `collect`
    /// can be fed known rows without touching the real one.
    fn scratch_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("opencode-usage-tracker-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).expect("scratch db");
        conn.execute_batch("CREATE TABLE session_message \
             (type TEXT, data TEXT, time_created INTEGER)")
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
        insert(&conn, today + DAY_MS + crate::AN_HOUR_IN_MS, "big-pickle", 999_999);
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

pub fn run(path: &PathBuf, days: Option<u32>, json: bool) -> anyhow::Result<()> {
    if json {
        // No bars to fit on screen, so JSON reports a week unless told
        // otherwise.
        let Some(stats) = collect(path, days.unwrap_or(JSON_DAYS)) else {
            anyhow::bail!("could not read {}", path.display());
        };
        println!("{}", render_json(&stats));
        return Ok(());
    }
    // The chart sizes its own day count to the terminal when --days is off.
    run_chart(path, days)?;
    Ok(())
}



