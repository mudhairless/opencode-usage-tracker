//! Local, human-editable configuration.
//!
//! The file lives at `$XDG_CONFIG_HOME/opencode-usage-tracker/config.toml`
//! (falling back to `~/.config/...`). Writes go through `toml_edit` so the
//! template's comments and layout survive every change. Nothing here ever
//! touches the network.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use chrono::Datelike;
use toml_edit::{DocumentMut, Item, Table};

/// Every key the CLI understands, in the order `--list` reports them.
const KEYS: &[&str] = &[
    "plan",
    "go.tier",
    "go.cycle_reset",
    "go.limit_notify",
    "balance",
    "balance_as_of",
    "balance_reminder",
    "yellow_percent",
    "red_percent",
];

/// Written verbatim when the config file is missing. The comments are the
/// file's documentation, so keep them clear and keep every key present.
const TEMPLATE: &str = r#"# Which Zen plan you are on. This selects what the tray tracks.
#
#   free    Zen's free tier: the traffic light counts free requests
#   go      a Zen Go subscription: usage against the plan's limits
#   credit  pay-as-you-go: usage is measured against the balance below
#
# Change it with:
#     opencode-usage-tracker config plan go
plan = "free"

# Remind you when the balance is stale (for example, after you add credit).
# Set this to false to silence the reminder entirely.
balance_reminder = true

# Percentage of the entered balance at which the tray light turns yellow
# (credit mode only). Must be greater than red_percent.
yellow_percent = 25

# Percentage of the entered balance at which the tray light turns red
# (credit mode only). Must be less than yellow_percent.
red_percent = 10

# Current Zen credit balance, in dollars.
# STRONGLY RECOMMENDED: set this with the CLI or the tray GUI, not by hand:
#     opencode-usage-tracker config balance 25.00
# Those paths stamp balance_as_of with the current time automatically, and
# the balance estimate and the freshness reminder are only as accurate as
# that stamp. If you edit by hand, update balance_as_of below too.
# balance = 0.00

# When the balance above was last set, as a UTC timestamp (RFC 3339).
# The CLI and GUI set this for you. Editing by hand? This command prints
# the current UTC time in exactly this format:
#     date -u +%FT%TZ
# balance_as_of = 2026-10-29T05:42:05Z

# Your Zen Go subscription tier. Limits differ per model, so this only
# selects the rule set:
#
#   go        Zen Go
#   go_plus   Zen Go Plus
#   go_teams  Zen Go for Teams
#
# Change it with:
#     opencode-usage-tracker config go.tier go_plus
[go]
tier = "go"

# The date your current Go billing cycle last reset (YYYY-MM-DD). When
# set, the monthly window is measured from the cycle start instead of
# over a rolling 30 days; the date rolls forward by whole months on its
# own, so you only set it once. Leave it unset if you do not know it.
#     opencode-usage-tracker config go.cycle_reset 2026-11-15
# cycle_reset = 2026-11-15

# Show a desktop notification when a model reaches its monthly limit.
# Set this to false to silence it.
limit_notify = true
"#;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Plan {
    #[default]
    Free,
    Go,
    Credit,
}

impl Plan {
    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Go => "go",
            Plan::Credit => "credit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "free" => Some(Plan::Free),
            "go" => Some(Plan::Go),
            "credit" => Some(Plan::Credit),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GoTier {
    #[default]
    Go,
    GoPlus,
    GoTeams,
}

impl GoTier {
    pub fn as_str(self) -> &'static str {
        match self {
            GoTier::Go => "go",
            GoTier::GoPlus => "go_plus",
            GoTier::GoTeams => "go_teams",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "go" => Some(GoTier::Go),
            "go_plus" => Some(GoTier::GoPlus),
            "go_teams" => Some(GoTier::GoTeams),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub plan: Plan,
    pub go_tier: GoTier,
    /// Raw `YYYY-MM-DD` string; see [`Config::cycle_reset`] for the parsed form.
    pub go_cycle_reset: Option<String>,
    pub go_limit_notify: bool,
    pub balance: Option<f64>,
    /// Raw RFC 3339 string; see [`Config::balance_time`] for the parsed form.
    pub balance_as_of: Option<String>,
    pub balance_reminder: bool,
    pub yellow_percent: f64,
    pub red_percent: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            plan: Plan::Free,
            go_tier: GoTier::Go,
            go_cycle_reset: None,
            go_limit_notify: true,
            balance: None,
            balance_as_of: None,
            balance_reminder: true,
            yellow_percent: 25.0,
            red_percent: 10.0,
        }
    }
}

impl Config {
    /// `balance_as_of` as a real timestamp, or `None` when unset or malformed.
    pub fn balance_time(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(self.balance_as_of.as_deref()?).ok()
    }

    /// The stored Go billing-cycle reset date, parsed, or `None` when
    /// unset or malformed.
    pub fn cycle_reset(&self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(self.go_cycle_reset.as_deref()?, "%Y-%m-%d").ok()
    }

    /// The start of the billing cycle containing `now_ms`, as epoch
    /// milliseconds at 00:00 UTC: the stored reset date rolled forward
    /// by whole months (see [`cycle_start`]). `None` when no reset
    /// date is set.
    pub fn cycle_start_ms(&self, now_ms: i64) -> Option<i64> {
        let reset = self.cycle_reset()?;
        let today = chrono::DateTime::from_timestamp_millis(now_ms)?.date_naive();
        Some(
            cycle_start(reset, today)
                .and_hms_opt(0, 0, 0)?
                .and_utc()
                .timestamp_millis(),
        )
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("opencode-usage-tracker").join("config.toml")
}

/// Read the config, creating it from [`TEMPLATE`] when it does not exist.
/// Only a genuine TOML syntax error is returned; individual bad or
/// misspelled values fall back to their defaults.
pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        ensure_file(path)?;
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path)?;
    let doc: DocumentMut = text.parse()?;
    Ok(from_doc(&doc))
}

/// [`load`] with every error swallowed into defaults. The tray uses this: a
/// broken config file must never take the tray down.
pub fn load_or_default(path: &Path) -> Config {
    load(path).unwrap_or_default()
}

/// Set `key` to `value`, editing the file in place so comments survive.
pub fn set_value(path: &Path, key: &str, raw: &str) -> Result<String> {
    check_key(key)?;
    ensure_file(path)?;
    let text = std::fs::read_to_string(path)?;
    let mut doc: DocumentMut = text.parse()?;
    let current = from_doc(&doc);

    let confirmation = match key {
        "plan" => {
            let plan =
                Plan::parse(raw).ok_or_else(|| anyhow!("plan must be one of: free, go, credit"))?;
            doc["plan"] = toml_edit::value(plan.as_str());
            format!("plan = {}", plan.as_str())
        }
        "go.tier" => {
            let tier = GoTier::parse(raw)
                .ok_or_else(|| anyhow!("go.tier must be one of: go, go_plus, go_teams"))?;
            go_table(&mut doc).insert("tier", toml_edit::value(tier.as_str()));
            format!("go.tier = {}", tier.as_str())
        }
        "go.cycle_reset" => {
            chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .map_err(|_| anyhow!("go.cycle_reset must be a date, e.g. 2026-11-15"))?;
            go_table(&mut doc).insert("cycle_reset", toml_edit::value(raw));
            format!("go.cycle_reset = {raw}")
        }
        "go.limit_notify" => {
            let on = match raw {
                "true" => true,
                "false" => false,
                _ => bail!("go.limit_notify must be true or false"),
            };
            go_table(&mut doc).insert("limit_notify", toml_edit::value(on));
            format!("go.limit_notify = {on}")
        }
        "balance" => {
            let balance = parse_money(raw)?;
            doc["balance"] = toml_edit::value(balance);
            doc["balance_as_of"] = toml_edit::value(now_datetime());
            format!("balance = {balance:.2}")
        }
        "balance_as_of" => {
            chrono::DateTime::parse_from_rfc3339(raw).map_err(|_| {
                anyhow!("balance_as_of must be an RFC 3339 timestamp, e.g. 2026-10-29T05:42:05Z")
            })?;
            doc["balance_as_of"] = toml_edit::value(raw);
            format!("balance_as_of = {raw}")
        }
        "balance_reminder" => {
            let on = match raw {
                "true" => true,
                "false" => false,
                _ => bail!("balance_reminder must be true or false"),
            };
            doc["balance_reminder"] = toml_edit::value(on);
            format!("balance_reminder = {on}")
        }
        "yellow_percent" => {
            let percent = parse_percent(raw)?;
            if current.red_percent >= percent {
                bail!(
                    "yellow_percent ({}) must be greater than red_percent ({})",
                    fmt_num(percent),
                    fmt_num(current.red_percent)
                );
            }
            doc["yellow_percent"] = toml_edit::value(percent);
            format!("yellow_percent = {}", fmt_num(percent))
        }
        "red_percent" => {
            let percent = parse_percent(raw)?;
            if percent >= current.yellow_percent {
                bail!(
                    "red_percent ({}) must be less than yellow_percent ({})",
                    fmt_num(percent),
                    fmt_num(current.yellow_percent)
                );
            }
            doc["red_percent"] = toml_edit::value(percent);
            format!("red_percent = {}", fmt_num(percent))
        }
        _ => unreachable!("check_key covers every key"),
    };

    std::fs::write(path, doc.to_string())?;
    Ok(confirmation)
}

/// The effective value of one key, formatted for display.
pub fn get_value(path: &Path, key: &str) -> Result<String> {
    check_key(key)?;
    let cfg = load(path)?;
    Ok(effective(&cfg, key))
}

/// Every key with its effective value, in the template's order.
pub fn list(path: &Path) -> Result<Vec<(String, String)>> {
    let cfg = load(path)?;
    Ok(KEYS
        .iter()
        .map(|key| ((*key).to_string(), effective(&cfg, key)))
        .collect())
}

/// The file's mtime in milliseconds; the tray polls this to notice edits.
pub fn modified_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// CLI entry point: `config --list`, `config --edit`, `config path`,
/// `config <key>`, `config <key> <value>`.
pub fn run(list_flag: bool, edit: bool, key: Option<String>, value: Option<String>) -> Result<()> {
    if (list_flag || edit) && (key.is_some() || value.is_some()) {
        bail!("--list and --edit cannot be combined with a key or value");
    }

    let path = config_path();

    if list_flag {
        for (key, value) in self::list(&path)? {
            println!("{key} = {value}");
        }
        return Ok(());
    }

    if edit {
        load(&path)?; // ensure the file (and its comments) exists
        let editor = std::env::var("EDITOR")
            .map_err(|_| anyhow!("$EDITOR is not set; set it to your preferred editor"))?;
        let mut parts = editor.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| anyhow!("$EDITOR is empty; set it to your preferred editor"))?;
        let status = std::process::Command::new(program)
            .args(parts)
            .arg(&path)
            .status()?;
        if !status.success() {
            bail!("editor exited with status {status}");
        }
        return Ok(());
    }

    match (key, value) {
        (None, None) => bail!("missing key; try `config --list` or `config <key>`"),
        (None, Some(_)) => bail!("missing key to go with the value"),
        (Some(key), None) if key == "path" => {
            println!("{}", path.display());
            Ok(())
        }
        (Some(key), None) => {
            println!("{}", get_value(&path, &key)?);
            Ok(())
        }
        (Some(key), Some(value)) => {
            println!("{}", set_value(&path, &key, &value)?);
            if key == "balance" {
                println!("balance_as_of = {}", get_value(&path, "balance_as_of")?);
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn check_key(key: &str) -> Result<()> {
    if KEYS.contains(&key) {
        Ok(())
    } else {
        bail!("unknown key `{key}`; valid keys: {}", KEYS.join(", "))
    }
}

fn ensure_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, TEMPLATE)?;
    Ok(())
}

fn from_doc(doc: &DocumentMut) -> Config {
    let mut cfg = Config::default();

    if let Some(plan) = doc
        .get("plan")
        .and_then(|i| i.as_str())
        .and_then(Plan::parse)
    {
        cfg.plan = plan;
    }
    if let Some(tier) = doc
        .get("go")
        .and_then(|i| i.as_table())
        .and_then(|t| t.get("tier"))
        .and_then(|i| i.as_str())
        .and_then(GoTier::parse)
    {
        cfg.go_tier = tier;
    }
    if let Some(table) = doc.get("go").and_then(|i| i.as_table()) {
        if let Some(item) = table.get("cycle_reset") {
            // Quoted strings are what this build writes; a hand edit may
            // use a native TOML date instead, so accept both.
            let raw = item
                .as_str()
                .map(str::to_string)
                .or_else(|| item.as_datetime().map(|d| d.to_string()));
            if let Some(raw) = raw
                && chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d").is_ok()
            {
                cfg.go_cycle_reset = Some(raw);
            }
        }
        if let Some(on) = table.get("limit_notify").and_then(|i| i.as_bool()) {
            cfg.go_limit_notify = on;
        }
    }
    if let Some(balance) = doc.get("balance").and_then(number)
        && balance >= 0.0
    {
        cfg.balance = Some(balance);
    }
    if let Some(item) = doc.get("balance_as_of") {
        // Native TOML datetimes are what this build writes; quoted RFC 3339
        // strings are what earlier builds wrote, so keep accepting both.
        if let Some(stamp) = item.as_datetime() {
            cfg.balance_as_of = Some(stamp.to_string());
        } else if let Some(stamp) = item.as_str() {
            cfg.balance_as_of = Some(stamp.to_string());
        }
    }
    if let Some(on) = doc.get("balance_reminder").and_then(|i| i.as_bool()) {
        cfg.balance_reminder = on;
    }
    if let Some(percent) = doc.get("yellow_percent").and_then(number)
        && percent > 0.0
        && percent <= 100.0
    {
        cfg.yellow_percent = percent;
    }
    if let Some(percent) = doc.get("red_percent").and_then(number)
        && percent > 0.0
        && percent <= 100.0
    {
        cfg.red_percent = percent;
    }

    cfg
}

fn effective(cfg: &Config, key: &str) -> String {
    match key {
        "plan" => cfg.plan.as_str().to_string(),
        "go.tier" => cfg.go_tier.as_str().to_string(),
        "go.cycle_reset" => cfg
            .go_cycle_reset
            .clone()
            .unwrap_or_else(|| "(unset)".to_string()),
        "go.limit_notify" => cfg.go_limit_notify.to_string(),
        "balance" => cfg
            .balance
            .map_or_else(|| "(unset)".to_string(), |b| format!("{b:.2}")),
        "balance_as_of" => cfg
            .balance_as_of
            .clone()
            .unwrap_or_else(|| "(unset)".to_string()),
        "balance_reminder" => cfg.balance_reminder.to_string(),
        "yellow_percent" => fmt_num(cfg.yellow_percent),
        "red_percent" => fmt_num(cfg.red_percent),
        _ => "(unknown)".to_string(),
    }
}

fn number(item: &Item) -> Option<f64> {
    item.as_float()
        .or_else(|| item.as_integer().map(|n| n as f64))
}

fn parse_money(raw: &str) -> Result<f64> {
    let balance: f64 = raw
        .trim()
        .parse()
        .map_err(|_| anyhow!("balance must be a decimal dollar amount, e.g. 12.50"))?;
    if !balance.is_finite() || balance < 0.0 {
        bail!("balance must be a decimal dollar amount >= 0, e.g. 12.50");
    }
    Ok(balance)
}

fn parse_percent(raw: &str) -> Result<f64> {
    let percent: f64 = raw
        .trim()
        .parse()
        .map_err(|_| anyhow!("expected a number in (0, 100], e.g. 25"))?;
    if !percent.is_finite() || percent <= 0.0 || percent > 100.0 {
        bail!("expected a number in (0, 100], e.g. 25");
    }
    Ok(percent)
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n}")
    }
}

/// The `[go]` table, created when missing or replaced when some other
/// type squats on the name.
fn go_table(doc: &mut DocumentMut) -> &mut Table {
    let item = doc.entry("go").or_insert(Item::Table(Table::new()));
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("go is a table")
}

/// The first day of the billing cycle containing `today`, given the
/// date a cycle last reset: `reset` advanced by whole months until it
/// is the latest reset not after `today`. Stepping is computed from
/// the stored date each time, so a reset on the 31st lands on the 31st
/// again after a short month instead of drifting.
fn cycle_start(reset: chrono::NaiveDate, today: chrono::NaiveDate) -> chrono::NaiveDate {
    let months = (today.year() - reset.year()) * 12 + (today.month() as i32 - reset.month() as i32);
    let candidate = add_months(reset, months);
    if candidate > today {
        add_months(reset, months - 1)
    } else {
        candidate
    }
}

fn add_months(date: chrono::NaiveDate, months: i32) -> chrono::NaiveDate {
    let total = date.year() * 12 + (date.month() as i32 - 1) + months;
    let year = total.div_euclid(12);
    let month = (total.rem_euclid(12) + 1) as u32;
    let day = date.day().min(days_in_month(year, month));
    chrono::NaiveDate::from_ymd_opt(year, month, day).expect("a clamped date is valid")
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_next =
        chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("a valid first");
    (first_next - chrono::Duration::days(1)).day()
}

fn now_datetime() -> toml_edit::Datetime {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
        .parse()
        .expect("the current UTC time is a valid TOML datetime")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway config path, removed first so each test starts clean.
    fn scratch_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "opencode-usage-tracker-config-{}-{name}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn first_load_creates_the_template_and_returns_defaults() {
        let path = scratch_path("first-load");
        let cfg = load(&path).expect("load");

        assert_eq!(cfg.plan, Plan::Free);
        assert_eq!(cfg.go_tier, GoTier::Go);
        assert!(cfg.go_cycle_reset.is_none());
        assert!(cfg.go_limit_notify);
        assert!(cfg.balance.is_none());
        assert!(cfg.balance_as_of.is_none());
        assert!(cfg.balance_reminder);
        assert_eq!(cfg.yellow_percent, 25.0);
        assert_eq!(cfg.red_percent, 10.0);

        let text = std::fs::read_to_string(&path).expect("template written");
        assert!(text.contains("STRONGLY RECOMMENDED"));
        assert!(text.contains("date -u +%FT%TZ"));
        assert!(
            text.contains("# balance = 0.00"),
            "a fresh file must not set a balance"
        );
    }

    #[test]
    fn explicit_zero_balance_roundtrips() {
        let path = scratch_path("zero");
        set_value(&path, "balance", "0").expect("set balance");
        assert_eq!(get_value(&path, "balance").expect("get"), "0.00");

        let cfg = load(&path).expect("reload");
        assert_eq!(cfg.balance, Some(0.0));
        assert!(cfg.balance_as_of.is_some());
    }

    #[test]
    fn string_form_balance_as_of_still_loads() {
        let path = scratch_path("stamp-string");
        let text = TEMPLATE.replace(
            "# balance = 0.00",
            "balance = 12.50\nbalance_as_of = \"2026-10-29T05:42:05Z\"",
        );
        std::fs::write(&path, text).expect("write");

        let cfg = load(&path).expect("load");
        assert_eq!(cfg.balance, Some(12.5));
        assert_eq!(cfg.balance_as_of.as_deref(), Some("2026-10-29T05:42:05Z"));
        assert_eq!(
            get_value(&path, "balance_as_of").expect("get"),
            "2026-10-29T05:42:05Z"
        );
    }

    #[test]
    fn balance_roundtrip_keeps_comments_and_stamps_the_time() {
        let path = scratch_path("balance");
        let confirmation = set_value(&path, "balance", "12.50").expect("set balance");
        assert!(confirmation.contains("12.50"));

        let text = std::fs::read_to_string(&path).expect("file");
        assert!(text.contains("STRONGLY RECOMMENDED"));
        assert!(
            text.contains("balance_as_of = 20"),
            "stamp must be written as a native TOML datetime"
        );
        assert!(
            !text.contains("balance_as_of = \""),
            "stamp must not be a quoted string"
        );

        assert_eq!(get_value(&path, "balance").expect("get"), "12.50");
        let stamp = get_value(&path, "balance_as_of").expect("get stamp");
        assert_ne!(stamp, "(unset)");

        let parsed = chrono::DateTime::parse_from_rfc3339(&stamp).expect("RFC 3339 stamp");
        let skew = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
        assert!(skew.abs() < 60, "stamp was {skew}s away from now");

        let cfg = load(&path).expect("reload");
        assert!(cfg.balance_time().is_some());
    }

    #[test]
    fn plan_tier_and_reminder_roundtrip() {
        let path = scratch_path("roundtrip");

        set_value(&path, "plan", "credit").expect("set plan");
        assert_eq!(get_value(&path, "plan").expect("get plan"), "credit");

        set_value(&path, "go.tier", "go_plus").expect("set tier");
        assert_eq!(get_value(&path, "go.tier").expect("get tier"), "go_plus");

        set_value(&path, "balance_reminder", "false").expect("set reminder");
        assert_eq!(
            get_value(&path, "balance_reminder").expect("get reminder"),
            "false"
        );

        let text = std::fs::read_to_string(&path).expect("file");
        assert!(text.contains("STRONGLY RECOMMENDED"));
        assert!(text.contains("[go]"));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let path = scratch_path("errors");

        assert!(set_value(&path, "nope", "x").is_err());
        assert!(set_value(&path, "balance", "-1").is_err());

        // Defaults are yellow 25 / red 10, so red >= yellow is invalid.
        assert!(set_value(&path, "red_percent", "25").is_err());
        assert!(set_value(&path, "yellow_percent", "5").is_err());

        assert!(set_value(&path, "plan", "enterprise").is_err());
        assert!(set_value(&path, "balance_reminder", "maybe").is_err());
    }

    #[test]
    fn load_or_default_survives_invalid_toml() {
        let path = scratch_path("invalid");
        std::fs::write(&path, "[broken\nthis is not = = toml").expect("write");

        let cfg = load_or_default(&path);
        assert_eq!(cfg.plan, Plan::Free);
        assert!(cfg.balance.is_none());
    }

    #[test]
    fn go_cycle_keys_roundtrip() {
        let path = scratch_path("go-cycle");

        set_value(&path, "go.cycle_reset", "2026-11-15").expect("set reset");
        assert_eq!(
            get_value(&path, "go.cycle_reset").expect("get reset"),
            "2026-11-15"
        );
        assert!(set_value(&path, "go.cycle_reset", "Nov 15").is_err());

        // The notification defaults to on, and turns off.
        assert_eq!(
            get_value(&path, "go.limit_notify").expect("get notify"),
            "true"
        );
        set_value(&path, "go.limit_notify", "false").expect("set notify");
        assert!(set_value(&path, "go.limit_notify", "maybe").is_err());

        let cfg = load(&path).expect("reload");
        assert!(!cfg.go_limit_notify);
        assert_eq!(
            cfg.cycle_reset(),
            chrono::NaiveDate::from_ymd_opt(2026, 11, 15)
        );

        let text = std::fs::read_to_string(&path).expect("file");
        assert!(text.contains("[go]"), "file:\n{text}");
    }

    #[test]
    fn cycle_start_rolls_forward_from_the_stored_date() {
        let date = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // Mid-cycle: the start is the most recent reset.
        assert_eq!(
            cycle_start(date(2026, 8, 15), date(2026, 11, 2)),
            date(2026, 10, 15)
        );
        // On the reset day itself, the new cycle has begun.
        assert_eq!(
            cycle_start(date(2026, 8, 15), date(2026, 11, 15)),
            date(2026, 11, 15)
        );
        // A stored date in the future steps back to the current cycle.
        assert_eq!(
            cycle_start(date(2026, 11, 15), date(2026, 11, 2)),
            date(2026, 10, 15)
        );
        // A 31st reset clamps into short months without drifting:
        // February starts on the 28th, March is back on the 31st.
        assert_eq!(
            cycle_start(date(2026, 1, 31), date(2026, 3, 15)),
            date(2026, 2, 28)
        );
        assert_eq!(
            cycle_start(date(2026, 1, 31), date(2026, 3, 31)),
            date(2026, 3, 31)
        );
    }
}
