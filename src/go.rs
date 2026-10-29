//! OpenCode Go subscription support: what each Go model costs and how
//! much of its plan limit local usage has consumed.
//!
//! Go rows in `session_message` (provider `opencode-go`) record `cost` 0
//! while usage is within limits — that is what the Zen inference API
//! reports, and the counted amount (`opencode.allowance`) is not stored
//! locally. So this module prices each request itself, from its recorded
//! tokens and the published per-model rates, and compares the sums over
//! rolling windows against the published monthly limits. The result is
//! an estimate of the server's list-price-based allowance, not a meter
//! reading.
//!
//! Prices and limits were captured from opencode.ai/go and the console
//! docs on Oct 29, 2026 (see GO-LIMITS-CAPTURE-2026-10-29.md in the
//! project notes). OpenCode changes both without ceremony; recapture
//! before trusting this table again. Model ids are the models.dev
//! catalog ids for the `opencode-go` provider, captured the same day.
//! Models missing from the table (new or promotional) report usage
//! without a price or a verdict rather than a guessed one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{config, open_db};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// One price row: rates per million tokens. `up_to` is the context size
/// (input + cache read + cache write) the row applies up to; the last
/// row for a model has `None` and catches everything above.
#[derive(Clone, Copy)]
struct PriceTier {
    up_to: Option<u64>,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

const fn tier(
    up_to: Option<u64>,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
) -> PriceTier {
    PriceTier {
        up_to,
        input,
        output,
        cache_read,
        cache_write,
    }
}

/// A Go model with its published monthly limits per plan (USD of usage).
/// `monthly_go: None` means no limit is published (the limited-time free
/// models) or none could be captured; either way there is no verdict.
pub struct GoModel {
    id: &'static str,
    name: &'static str,
    monthly_go: Option<f64>,
    monthly_plus: Option<f64>,
    /// DeepSeek's published peak rates are exactly twice the off-peak
    /// ones; peak is 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday.
    peak_double: bool,
    /// Requests before this instant (ms) price at half the listed rates
    /// (a published launch discount with a known end).
    half_until_ms: Option<i64>,
    tiers: &'static [PriceTier],
}

/// Shorthand for the common case: one flat price row, no peak or
/// discount adjustments. A macro so the entry expands inside the
/// `MODELS` initializer itself, where the price row can be promoted to
/// a static instead of referencing a helper's local. The price row is
/// captured as raw token trees, not `:expr`: an expression fragment
/// cannot contain the `tier(...)` call's commas.
macro_rules! flat {
    ($id:expr, $name:expr, $go:expr, $plus:expr, $($rates:tt)*) => {
        GoModel {
            id: $id,
            name: $name,
            monthly_go: $go,
            monthly_plus: $plus,
            peak_double: false,
            half_until_ms: None,
            tiers: &[$($rates)*],
        }
    };
}

static MODELS: &[GoModel] = &[
    flat!(
        "glm-5.3-flash",
        "GLM-5.3-Flash",
        Some(60.0),
        Some(180.0),
        tier(None, 0.15, 0.50, 0.03, 0.0),
    ),
    flat!(
        "glm-5.3",
        "GLM-5.3",
        Some(15.0),
        None,
        tier(None, 1.40, 4.40, 0.26, 0.0),
    ),
    flat!(
        "glm-5.2",
        "GLM-5.2",
        Some(60.0),
        None,
        tier(None, 1.40, 4.40, 0.26, 0.0),
    ),
    flat!(
        "kimi-k3.1-pro",
        "Kimi K3.1 Pro",
        Some(15.0),
        Some(60.0),
        tier(None, 2.00, 10.00, 0.20, 2.00),
    ),
    flat!(
        "kimi-k3",
        "Kimi K3",
        Some(15.0),
        None,
        tier(None, 3.00, 15.00, 0.30, 3.00),
    ),
    flat!(
        "kimi-k2.7-code",
        "Kimi K2.7 Code",
        Some(60.0),
        Some(180.0),
        tier(None, 0.95, 4.00, 0.19, 0.0),
    ),
    flat!(
        "longcat-2.0",
        "LongCat-2.0",
        Some(60.0),
        None,
        tier(None, 0.30, 1.20, 0.006, 0.0),
    ),
    flat!(
        "step-5-preview",
        "Step 5 Preview",
        Some(15.0),
        None,
        tier(None, 1.00, 2.70, 0.10, 0.0),
    ),
    flat!(
        "beam-free",
        "Beam Free",
        None,
        None,
        tier(None, 0.0, 0.0, 0.0, 0.0),
    ),
    flat!(
        "iceman-contributor-free",
        "Iceman Contributor Free",
        None,
        None,
        tier(None, 0.0, 0.0, 0.0, 0.0),
    ),
    flat!(
        "mimo-v2.6-flash",
        "MiMo-V2.6-Flash",
        Some(60.0),
        Some(120.0),
        tier(None, 0.14, 0.28, 0.0028, 0.0),
    ),
    flat!(
        "mimo-v2.6-pro",
        "MiMo-V2.6-Pro",
        Some(15.0),
        None,
        tier(None, 0.435, 0.87, 0.003625, 0.0),
    ),
    flat!(
        "mimo-v2.5",
        "MiMo-V2.5",
        Some(60.0),
        None,
        tier(None, 0.14, 0.28, 0.0028, 0.0),
    ),
    flat!(
        "mimo-v2.5-pro",
        "MiMo-V2.5-Pro",
        Some(15.0),
        None,
        tier(None, 0.435, 0.87, 0.003625, 0.0),
    ),
    flat!(
        "minimax-m3.1-flash",
        "MiniMax M3.1 Flash",
        Some(20.0),
        Some(80.0),
        tier(None, 0.15, 0.60, 0.015, 0.0),
    ),
    GoModel {
        id: "minimax-m3",
        name: "MiniMax M3",
        monthly_go: Some(60.0),
        monthly_plus: Some(180.0),
        peak_double: false,
        half_until_ms: None,
        tiers: &[
            tier(Some(512_000), 0.30, 1.20, 0.06, 0.0),
            tier(None, 0.60, 2.40, 0.12, 0.0),
        ],
    },
    flat!(
        "muse-spark-1.3-contributor",
        "Muse Spark 1.3 Contributor",
        Some(60.0),
        Some(120.0),
        tier(None, 0.10, 0.20, 0.002, 0.0),
    ),
    flat!(
        "muse-spark-1.2-contributor",
        "Muse Spark 1.2 Contributor",
        Some(60.0),
        None,
        tier(None, 0.10, 0.20, 0.002, 0.0),
    ),
    flat!(
        "qwen3.8-max",
        "Qwen3.8 Max",
        Some(15.0),
        None,
        tier(None, 2.00, 6.00, 0.25, 2.50),
    ),
    flat!(
        "qwen3.8-flash",
        "Qwen3.8 Flash",
        Some(30.0),
        None,
        tier(None, 0.15, 0.47, 0.016, 0.20),
    ),
    GoModel {
        id: "qwen3.7-plus",
        name: "Qwen3.7 Plus",
        monthly_go: Some(60.0),
        monthly_plus: Some(180.0),
        peak_double: false,
        half_until_ms: None,
        tiers: &[
            tier(Some(256_000), 0.40, 1.60, 0.04, 0.50),
            tier(None, 1.20, 4.80, 0.12, 1.50),
        ],
    },
    GoModel {
        id: "deepseek-v4.1-flash",
        name: "DeepSeek V4.1 Flash",
        monthly_go: Some(80.0),
        monthly_plus: Some(160.0),
        peak_double: true,
        half_until_ms: None,
        tiers: &[tier(None, 0.15, 0.60, 0.003, 0.0)],
    },
    GoModel {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        monthly_go: Some(15.0),
        monthly_plus: None,
        peak_double: true,
        half_until_ms: None,
        tiers: &[tier(None, 0.66, 1.98, 0.022, 0.0)],
    },
    flat!(
        "hy4-preview",
        "Hy4 preview",
        Some(30.0),
        None,
        tier(None, 0.834, 2.501, 0.042, 0.0),
    ),
    flat!(
        "hy3",
        "Hy3",
        Some(60.0),
        None,
        tier(None, 0.14, 0.58, 0.035, 0.0),
    ),
    GoModel {
        id: "grok-4.7",
        name: "Grok 4.7",
        monthly_go: Some(15.0),
        monthly_plus: None,
        peak_double: false,
        half_until_ms: None,
        tiers: &[
            tier(Some(200_000), 2.00, 6.00, 0.50, 0.0),
            tier(None, 4.00, 12.00, 1.00, 0.0),
        ],
    },
    GoModel {
        id: "gpt-6-luna",
        name: "GPT 6 Luna",
        monthly_go: Some(15.0),
        monthly_plus: Some(60.0),
        peak_double: false,
        half_until_ms: None,
        tiers: &[
            tier(Some(272_000), 0.10, 0.50, 0.01, 0.125),
            tier(None, 0.20, 0.75, 0.02, 0.25),
        ],
    },
    GoModel {
        id: "gpt-5.6-luna",
        name: "GPT 5.6 Luna",
        monthly_go: Some(15.0),
        monthly_plus: None,
        peak_double: false,
        half_until_ms: None,
        tiers: &[
            tier(Some(272_000), 0.20, 1.20, 0.02, 0.25),
            tier(None, 0.40, 1.80, 0.04, 0.50),
        ],
    },
    GoModel {
        id: "claude-haiku-5-5",
        name: "Claude Haiku 5.5",
        monthly_go: Some(15.0),
        monthly_plus: Some(60.0),
        peak_double: false,
        half_until_ms: None,
        tiers: &[
            tier(Some(100_000), 0.10, 0.50, 0.01, 0.125),
            tier(None, 0.50, 2.50, 0.05, 0.625),
        ],
    },
    GoModel {
        id: "gemini-4-argon",
        name: "Gemini 4 Argon",
        monthly_go: Some(30.0),
        monthly_plus: Some(120.0),
        peak_double: false,
        // Listed rates are the full ones; a 50% launch discount runs
        // through Nov 30, 2026 (docs, captured Oct 29, 2026).
        half_until_ms: Some(1_796_083_200_000), // 2026-12-01T00:00:00Z
        tiers: &[tier(None, 4.00, 20.00, 0.20, 0.0)],
    },
];

fn model(id: &str) -> Option<&'static GoModel> {
    MODELS.iter().find(|m| m.id == id)
}

/// A model's monthly limit under a plan. Go Teams is published as Go
/// plus $5 per model; anything else unpublished stays unknown.
fn monthly_limit(model: &GoModel, plan: config::GoTier) -> Option<f64> {
    match plan {
        config::GoTier::Go => model.monthly_go,
        config::GoTier::GoPlus => model.monthly_plus,
        config::GoTier::GoTeams => model.monthly_go.map(|m| m + 5.0),
    }
}

/// The five token buckets a message records, as counts.
#[derive(Clone, Copy, Default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

fn tokens_of(v: &serde_json::Value) -> Tokens {
    let bucket =
        |n: &serde_json::Value, key: &str| n.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    let Some(t) = v.get("tokens") else {
        return Tokens::default();
    };
    Tokens {
        input: bucket(t, "input"),
        output: bucket(t, "output"),
        reasoning: bucket(t, "reasoning"),
        cache_read: t.get("cache").map_or(0, |c| bucket(c, "read")),
        cache_write: t.get("cache").map_or(0, |c| bucket(c, "write")),
    }
}

/// DeepSeek peak: 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday.
fn is_peak(ms: i64) -> bool {
    let days = ms.div_euclid(DAY_MS);
    // Epoch day 0 was a Thursday; with Mon=0..Sun=6 the weekday is
    // (days + 3) mod 7.
    let weekday = (days + 3).rem_euclid(7);
    if weekday > 4 {
        return false;
    }
    let hour = ms.rem_euclid(DAY_MS) / HOUR_MS;
    (1..4).contains(&hour) || (6..10).contains(&hour)
}

/// What one request counted, in dollars, at the published rates: input
/// rate on non-cached input, output rate on output plus reasoning, and
/// the cache rates, per million tokens — the same shape OpenCode's own
/// catalog calculation uses. The context size (input + cache) picks the
/// price tier where a model has more than one.
pub fn price(model: &GoModel, tokens: Tokens, created_ms: i64) -> f64 {
    let context = tokens.input + tokens.cache_read + tokens.cache_write;
    let rates = model
        .tiers
        .iter()
        .find(|t| t.up_to.is_none_or(|max| context <= max))
        .unwrap_or_else(|| model.tiers.last().expect("every model has a price tier"));
    let mut value = (tokens.input as f64 * rates.input
        + (tokens.output + tokens.reasoning) as f64 * rates.output
        + tokens.cache_read as f64 * rates.cache_read
        + tokens.cache_write as f64 * rates.cache_write)
        / 1_000_000.0;
    if model.peak_double && is_peak(created_ms) {
        value *= 2.0;
    }
    if model.half_until_ms.is_some_and(|until| created_ms < until) {
        value *= 0.5;
    }
    value
}

/// One model's priced usage over the three rolling windows, with the
/// monthly limit for the configured plan. `priced` is false when the
/// model is outside the published table: its usage is real but cannot
/// be turned into dollars here.
#[derive(Clone, Debug, Default)]
pub struct ModelUsage {
    pub name: String,
    pub priced: bool,
    pub used_5h: f64,
    pub used_week: f64,
    pub used_month: f64,
    pub monthly_limit: Option<f64>,
}

impl ModelUsage {
    /// The (used, window limit) pair for a window label from `worst`.
    pub fn window_figures(&self, window: &str) -> (f64, f64) {
        let monthly = self.monthly_limit.unwrap_or(0.0);
        match window {
            "last 5 hours" => (self.used_5h, monthly * 0.2),
            "last 7 days" => (self.used_week, monthly * 0.5),
            _ => (self.used_month, monthly),
        }
    }

    /// The tightest window: the highest used/limit ratio and its label,
    /// or `None` when no limit is published for this model.
    pub fn worst(&self) -> Option<(f64, &'static str)> {
        let monthly = self.monthly_limit?;
        let mut best: Option<(f64, &'static str)> = None;
        for (used, fraction, label) in [
            (self.used_5h, 0.2, "last 5 hours"),
            (self.used_week, 0.5, "last 7 days"),
            (self.used_month, 1.0, "last 30 days"),
        ] {
            let ratio = used / (monthly * fraction);
            if best.is_none_or(|(r, _)| ratio > r) {
                best = Some((ratio, label));
            }
        }
        best
    }
}

/// The tightest (model, window) across all usage: the figure the tray
/// light follows in Go mode.
pub fn worst_ratio(usages: &[ModelUsage]) -> Option<(f64, &ModelUsage, &'static str)> {
    let mut best: Option<(f64, &ModelUsage, &'static str)> = None;
    for usage in usages {
        if let Some((ratio, label)) = usage.worst()
            && best.is_none_or(|(r, _, _)| ratio > r)
        {
            best = Some((ratio, usage, label));
        }
    }
    best
}

/// Price the last 30 days of Go usage from the local database, grouped
/// by model, oldest window outermost. Mirrors the tracker's counting
/// rules: assistant rows that never finished are skipped, compaction
/// rows count as-is. `None` when the database can't be read.
pub fn collect_usage(path: &PathBuf, plan: config::GoTier, now_ms: i64) -> Option<Vec<ModelUsage>> {
    let conn = open_db(path)?;
    let month_start = now_ms - 30 * DAY_MS;
    let week_start = now_ms - 7 * DAY_MS;
    let five_h_start = now_ms - 5 * HOUR_MS;
    let mut stmt = conn
        .prepare(
            "SELECT type, data, time_created FROM session_message \
             WHERE type IN ('assistant', 'compaction') AND time_created >= ?1",
        )
        .ok()?;
    let rows = stmt
        .query_map([month_start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .ok()?;

    let mut by_model: BTreeMap<String, ModelUsage> = BTreeMap::new();
    for row in rows {
        let Ok((msg_type, data, created)) = row else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let model_json = v.get("model");
        if model_json
            .and_then(|m| m.get("providerID"))
            .and_then(|p| p.as_str())
            != Some("opencode-go")
        {
            continue;
        }
        if msg_type == "assistant"
            && v.pointer("/time/completed").is_none()
            && v.get("finish").is_none()
        {
            continue;
        }
        let Some(id) = model_json
            .and_then(|m| m.get("id"))
            .and_then(|i| i.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let known = model(&id);
        let usage = by_model.entry(id.clone()).or_insert_with(|| ModelUsage {
            name: known.map_or_else(|| id.clone(), |m| m.name.to_string()),
            priced: known.is_some(),
            monthly_limit: known.and_then(|m| monthly_limit(m, plan)),
            ..ModelUsage::default()
        });
        let Some(known) = known else {
            continue;
        };
        let value = price(known, tokens_of(&v), created);
        usage.used_month += value;
        if created >= week_start {
            usage.used_week += value;
        }
        if created >= five_h_start {
            usage.used_5h += value;
        }
    }
    Some(by_model.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn ms(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    fn tokens(
        input: u64,
        output: u64,
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> Tokens {
        Tokens {
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn flat_pricing_uses_every_bucket() {
        let m = model("kimi-k2.7-code").unwrap();
        // $0.95 in + $4.00 out + $2.00 reasoning (500K at the output
        // rate) + $0.19 cache read.
        let p = price(
            m,
            tokens(1_000_000, 1_000_000, 500_000, 1_000_000, 0),
            ms("2026-10-29T12:00:00Z"),
        );
        close(p, 7.14);
    }

    #[test]
    fn context_size_picks_the_price_tier() {
        let m = model("claude-haiku-5-5").unwrap();
        // At the 100K threshold the lower tier still applies...
        close(
            price(m, tokens(100_000, 0, 0, 0, 0), ms("2026-10-29T12:00:00Z")),
            0.01,
        );
        // ...and above it the higher input rate does.
        close(
            price(m, tokens(200_000, 0, 0, 0, 0), ms("2026-10-29T12:00:00Z")),
            0.10,
        );
        // Cache reads count toward the context that picks the tier:
        // 10K fresh + 100K cached is over the threshold, so both price
        // at the higher tier's rates.
        close(
            price(
                m,
                tokens(10_000, 0, 0, 100_000, 0),
                ms("2026-10-29T12:00:00Z"),
            ),
            0.01,
        );
    }

    #[test]
    fn deepseek_peak_doubles_weekday_early_hours() {
        let m = model("deepseek-v4.1-flash").unwrap();
        let one_million_in = tokens(1_000_000, 0, 0, 0, 0);
        // Monday 02:00 UTC is peak: $0.15 doubled.
        close(price(m, one_million_in, ms("2026-10-26T02:00:00Z")), 0.30);
        // Monday 12:00 UTC is off-peak.
        close(price(m, one_million_in, ms("2026-10-26T12:00:00Z")), 0.15);
        // Saturday 02:00 UTC is off-peak despite the hour.
        close(price(m, one_million_in, ms("2026-10-31T02:00:00Z")), 0.15);
    }

    #[test]
    fn argon_discount_halves_until_december() {
        let m = model("gemini-4-argon").unwrap();
        let one_million_in = tokens(1_000_000, 0, 0, 0, 0);
        close(price(m, one_million_in, ms("2026-10-29T12:00:00Z")), 2.00);
        close(price(m, one_million_in, ms("2026-12-02T12:00:00Z")), 4.00);
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert!(model("cat-jam-free").is_none());
        assert!(model("big-pickle").is_none());
    }

    fn scratch_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "opencode-usage-tracker-go-{}-{name}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).expect("scratch db");
        conn.execute_batch(
            "CREATE TABLE session_message (type TEXT, data TEXT, time_created INTEGER)",
        )
        .expect("schema");
        path
    }

    fn insert(conn: &Connection, at_ms: i64, provider: &str, id: &str, finished: bool) {
        let mut data = serde_json::json!({
            "model": { "providerID": provider, "id": id },
            "tokens": { "input": 1_000_000, "output": 0, "reasoning": 0,
                        "cache": { "read": 0, "write": 0 } },
            "time": { "created": at_ms },
        });
        if finished {
            data["time"]["completed"] = serde_json::json!(1);
            data["finish"] = serde_json::json!("stop");
        }
        conn.execute(
            "INSERT INTO session_message (type, data, time_created) VALUES ('assistant', ?1, ?2)",
            rusqlite::params![data.to_string(), at_ms],
        )
        .expect("insert");
    }

    #[test]
    fn collect_usage_buckets_rows_by_window() {
        let path = scratch_db("windows");
        let conn = Connection::open(&path).unwrap();
        let now = ms("2026-10-29T12:00:00Z");
        // One $0.95 Kimi request in each window band.
        insert(&conn, now - HOUR_MS, "opencode-go", "kimi-k2.7-code", true);
        insert(
            &conn,
            now - 3 * DAY_MS,
            "opencode-go",
            "kimi-k2.7-code",
            true,
        );
        insert(
            &conn,
            now - 20 * DAY_MS,
            "opencode-go",
            "kimi-k2.7-code",
            true,
        );
        // Outside the month, unfinished, wrong provider: all excluded.
        insert(
            &conn,
            now - 40 * DAY_MS,
            "opencode-go",
            "kimi-k2.7-code",
            true,
        );
        insert(&conn, now - HOUR_MS, "opencode-go", "kimi-k2.7-code", false);
        insert(&conn, now - HOUR_MS, "opencode", "big-pickle", true);
        // A Go model outside the published table shows up unpriced.
        insert(&conn, now - HOUR_MS, "opencode-go", "cat-jam-free", true);
        drop(conn);

        let usages = collect_usage(&path, config::GoTier::Go, now).expect("collect");
        let _ = std::fs::remove_file(&path);
        assert_eq!(usages.len(), 2);
        let kimi = usages.iter().find(|u| u.name == "Kimi K2.7 Code").unwrap();
        close(kimi.used_5h, 0.95);
        close(kimi.used_week, 1.90);
        close(kimi.used_month, 2.85);
        assert_eq!(kimi.monthly_limit, Some(60.0));
        let cat = usages.iter().find(|u| u.name == "cat-jam-free").unwrap();
        assert!(!cat.priced);
        assert_eq!(cat.monthly_limit, None);
        assert!(cat.worst().is_none());

        // The same usage under the other plans: Teams adds $5, Plus uses
        // its own published figure for this model.
        let path2 = scratch_db("tiers");
        let conn = Connection::open(&path2).unwrap();
        insert(&conn, now - HOUR_MS, "opencode-go", "kimi-k2.7-code", true);
        drop(conn);
        let teams = collect_usage(&path2, config::GoTier::GoTeams, now).expect("collect");
        assert_eq!(teams[0].monthly_limit, Some(65.0));
        let plus = collect_usage(&path2, config::GoTier::GoPlus, now).expect("collect");
        assert_eq!(plus[0].monthly_limit, Some(180.0));
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn worst_ratio_picks_the_tightest_window() {
        let mut a = ModelUsage {
            name: "A".to_string(),
            priced: true,
            used_5h: 1.0,
            used_week: 20.0,
            used_month: 30.0,
            monthly_limit: Some(60.0),
        };
        // A's tightest is the week: 20 / (60 * 0.5) = 0.667.
        let (ratio, label) = a.worst().unwrap();
        close(ratio, 20.0 / 30.0);
        assert_eq!(label, "last 7 days");
        let (used, limit) = a.window_figures("last 7 days");
        close(used, 20.0);
        close(limit, 30.0);
        let b = ModelUsage {
            monthly_limit: None,
            ..a.clone()
        };
        a.used_5h = 12.0; // exactly the 5-hour limit of 12
        let usages = [a, b];
        let (ratio, usage, label) = worst_ratio(&usages).unwrap();
        close(ratio, 1.0);
        assert_eq!(usage.name, "A");
        assert_eq!(label, "last 5 hours");
    }
}
