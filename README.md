# opencode-usage-tracker

[![Rust](https://github.com/mudhairless/opencode-usage-tracker/actions/workflows/rust.yml/badge.svg)](https://github.com/mudhairless/opencode-usage-tracker/actions/workflows/rust.yml)

A KDE Plasma and GNOME tray icon that tracks your OpenCode usage. By default it counts today's OpenCode Zen free-tier requests; it can also follow a Zen credit balance or an OpenCode Go subscription (see [Paid plans](#paid-plans)). It reads OpenCode's local SQLite database every 30 seconds and colors the icon to match.

On the default free plan, the color follows today's free-tier requests:

| Icon color | Requests today |
| --- | --- |
| Green | fewer than 50 |
| Yellow | 50 to 74 |
| Red | 75 or more |
| Gray | the database could not be read |

The bands are conservative on purpose: the free allowance is not a fixed number (see [Counting](#counting)), and recent cutoffs have arrived below 90 requests in a day. Red means you are in the range where a cutoff has actually been seen, not that a limit is known.

On the paid plans the light follows that plan's own resource instead: the estimated credit balance left, or how much of a Go model's tightest window limit is used.

The Zen day resets at 00:00 UTC. The tooltip and the tray menu show the reset time in your local timezone.

The tracker runs no INSERT, UPDATE, or DELETE statements, and it makes no network requests. Every number comes from the local OpenCode database.

The `stats` subcommand charts usage in the terminal: per UTC day, per model, or both, with token and cost breakdowns.

## Quick start

```sh
git clone https://github.com/mudhairless/opencode-usage-tracker.git
cd opencode-usage-tracker
cargo build --release
./target/release/opencode-usage-tracker install --autostart
./target/release/opencode-usage-tracker
```

That is the whole setup for the default free plan: no configuration, no accounts, no network. The icon appears in the tray (on GNOME, enable the extension named in [Desktop support](#desktop-support) first, or nothing will show). Green means today's free-tier use is light, yellow means it is climbing, red means it is heavy; the tooltip and the right-click menu carry the exact counts. The paid plans need one setting each; see [Paid plans](#paid-plans).

## Requirements

- Linux with a system tray that implements StatusNotifierItem. Development targets KDE Plasma. See [Desktop support](#desktop-support) for other desktops, including GNOME.
- Rust 1.88 or newer, because the code uses edition 2024 and let-chains.
- A C compiler, because the bundled SQLite in `rusqlite` builds from source.
- OpenCode used on the same machine, so that its database exists.

## Desktop support

The tray uses the StatusNotifierItem protocol. It works natively on KDE Plasma, XFCE, LXQt, MATE, and Cinnamon, and in standalone bars such as waybar.

GNOME does not implement StatusNotifierItem on its own. Install and enable the **AppIndicator and KStatusNotifierItem Support** extension (preinstalled on Ubuntu) or no tray icon will appear at all. The extension does not implement tooltips, so on GNOME the right-click menu is the readout: it carries the same lines as the tooltip.

Pantheon's wingpanel is not supported.

Desktop notifications (the credit balance reminder) work on KDE and GNOME. On a notification daemon without action button support, the reminder still shows its text, including the command that sets the balance.

## Build

```sh
git clone https://github.com/mudhairless/opencode-usage-tracker.git
cd opencode-usage-tracker
cargo build --release
```

The binary is written to `target/release/opencode-usage-tracker`.

## Install

Build the binary, then run the installer from the same checkout:

```sh
cargo build --release
./target/release/opencode-usage-tracker install --autostart
```

`install` copies the executable and writes a menu entry whose `Exec=` points at the installed copy. `--autostart` also starts the tray when you log in.

To install for the whole machine, add `--system` and run the command with sudo:

```sh
sudo ./target/release/opencode-usage-tracker install --system --autostart
```

The autostart entry goes to the home directory of the user who ran the command, even under sudo. A system-wide install does not start the tray for other accounts on the machine.

If you run `install` with sudo but forget `--system`, the command stops and tells you to drop sudo. Without `--system` it touches only your own files.

### Installed files

| File | User install | System install |
| --- | --- | --- |
| Executable | `~/.local/bin/opencode-usage-tracker` | `/usr/local/bin/opencode-usage-tracker` |
| Menu entry | `~/.local/share/applications/opencode-usage-tracker.desktop` | `/usr/share/applications/opencode-usage-tracker.desktop` |
| Autostart entry | `~/.config/autostart/opencode-usage-tracker.desktop` | the same path, for the invoking user |

A user install honors `XDG_DATA_HOME` and `XDG_CONFIG_HOME` when they hold absolute paths.

### Install options

| Option | Effect |
| --- | --- |
| `--system` | Install under `/usr/local/bin` and `/usr/share/applications`. Requires root. |
| `--autostart` | Write the autostart entry. Without this option, `install` removes an existing entry. |
| `--from FILE` | Install FILE instead of the running executable. |

## Uninstall

```sh
./target/release/opencode-usage-tracker uninstall
sudo ./target/release/opencode-usage-tracker uninstall --system
```

`uninstall` removes the executable, the menu entry, and the autostart entry. It reports each file that is already absent and exits with status 0.

## Command line

| Command | Effect |
| --- | --- |
| `opencode-usage-tracker` | Run the tray. |
| `opencode-usage-tracker --once` | Print the tooltip text and exit. |
| `opencode-usage-tracker install` | Install for the current user. |
| `opencode-usage-tracker config` | Read or change configuration (see [Configuration](#configuration)). |
| `opencode-usage-tracker stats` | Chart usage in the terminal (see [The stats chart](#the-stats-chart)). |
| `opencode-usage-tracker uninstall` | Remove an install for the current user. |

`--once` prints the same lines as the tooltip:

```
$ opencode-usage-tracker --once
OpenCode Zen Free Tier Usage
250 requests today
43 requests last hour
9.6M tokens today
612 requests last 3 days · 31.2M tokens
Resets at 7:00 PM
```

If the database cannot be read, `--once` prints `usage unavailable (database unreadable)` and exits with status 1.

### The tray item

- Hovering shows today's free requests, the last hour's count, today's tokens, the last 3 days' totals, and the next reset time. On a paid plan it adds that plan's line: the estimated credit balance left, or the Go model closest to a window limit.
- The menu carries the same figures in full: a summary, a per-day breakdown of the last 3 days, the paid plan's detail, a **Configure** submenu, **Refresh now**, and **Quit**.
- Clicking the icon refreshes the counts.
- While a request is streaming, the tracker polls once a second and animates the icon.
- While OpenCode is not running, the icon is green and Plasma moves the item to the hidden tray area. The tooltip adds an `OpenCode not running` line.

### The stats chart

`stats` draws one bar per row of its report. With no options the rows are UTC days; the grouping options below make them models, or days crossed with models. Bars are eight columns wide with a one column gap, wider when a label needs the room, so the default day count is whichever whole number of bars fits across the terminal. Each bar prints its value at its base (tokens as `47.21M`, request counts whole, cost in dollars), and a box in the top-right corner names the metric on show.

| Key | Effect |
| --- | --- |
| `←` and `→`, or `h` and `l` | Slide the window one row. Works only when more rows are loaded than fit on screen. |
| `Tab` | Cycle the metric: tokens, requests, and cost with the default fields. A narrower `--fields` selection cycles what it carries. |
| `b` | Switch between bars and a line. |
| `q` or `Esc` | Quit. |

### stats options

The options compose: pick a range, a grouping, the metric columns, and any filters.

| Option | Effect |
| --- | --- |
| `--days N` | The last N days ending today, 1 to 365. Defaults to the most days whose bars fit across the terminal, and to 7 with `--json` or `--text`. Cannot be combined with `--from`/`--to`. |
| `--from DATE` | Start of the range as a UTC date (`YYYY-MM-DD`). |
| `--to DATE` | End of the range as a UTC date, inclusive. With only one of `--from`/`--to`, the other end defaults from it. |
| `--by LIST` | Grouping: `day`, `model`, `day,model`, or `model,day`. Default `day`. |
| `--fields LIST` | Metric columns: `requests`, `tokens`, `input`, `output`, `reasoning`, `cache-read`, `cache-write`, `cache-ratio`, `cost`. `tokens` selects the total plus all five token buckets; `cache-ratio` is the share of context tokens served from cache (cache reads over cache reads plus fresh input). Default: all. |
| `--model ID` | Only this model. `*` and `?` are wildcards. |
| `--paid` | Only billed rows (recorded cost above zero). |
| `--free` | Only free-pool rows. |
| `--text` | Print the report as a table instead of the chart. |
| `--json` | Print the report as JSON instead of the chart. |

Every row classifies three ways: `zen-free` (the shared free pool), `billed` (recorded cost above zero), and `other` (local providers, third-party zero-cost rows, and rows with no model, grouped as `(unknown)`). Unfiltered output carries the category on each row. `other` usage is never counted as free-pool usage or as paid spend, and rows with missing cost or token fields count those as zero.

Worked examples:

```sh
opencode-usage-tracker stats --by day --fields cost --days 30   # daily spend, last month
opencode-usage-tracker stats --model big-pickle --fields tokens --days 7
opencode-usage-tracker stats --by day,model --paid --days 14     # paid models by day
```

`--json` prints one object: the rows and the totals over the whole filtered set. Totals always carry every metric, even when `--fields` narrows the rows. The object also names the busiest single minute of the range as `peak_minute` (its start as an RFC 3339 timestamp and its request count, or `null` when the range is empty), and the text table prints the same figure under the totals. It separates a heavy day from a bursty one: the same daily total reads very differently when most of its requests landed inside a few minutes.

```
$ opencode-usage-tracker stats --days 1 --json
{"rows":[{"cache_read":11000000,"cache_write":405942,"category":"zen-free","cost":0.0,"date":"2026-10-18","input":30000000,"output":2000000,"reasoning":1000000,"requests":525,"tokens":44405942},{"cache_read":0,"cache_write":0,"category":"billed","cost":0.42,"date":"2026-10-18","input":80000,"output":10000,"reasoning":0,"requests":3,"tokens":90000}],"totals":{"cache_read":11000000,"cache_write":405942,"cost":0.42,"input":30080000,"output":2010000,"reasoning":1000000,"requests":528,"tokens":44495942}}
```

## Counting

The tracker counts rows in `$HOME/.local/share/opencode/opencode.db`, table `session_message`. A row counts when all of these hold:

- The row was created after 00:00 UTC today.
- The type is `assistant` or `compaction`.
- The model provider is `opencode`.
- The model is free: the id is `big-pickle`, or the id ends in `-free`.
- For `assistant` rows only, the message finished. Rows that are still streaming or were aborted do not count.

The tracker opens the database read-write so that a pending WAL is replayed, then runs only SELECT statements. Set `OPENCODE_DB` to count from a different file.

A word on what the counts mean: OpenCode Support describes the free allowance as a pool shared by every device behind your network's public IP address, reset at 00:00 UTC, and not a fixed number. Heavy use over recent days can lower it for a while. The counts here are exact, but no count can promise where the wall is; treat the light as a gauge of your own recent load, which is why the tray also shows the last 3 days.

## Configuration

Settings live in a TOML file at `$XDG_CONFIG_HOME/opencode-usage-tracker/config.toml` (usually `~/.config/opencode-usage-tracker/config.toml`). The file is created from a fully commented template on first use: the comments document every option, and the tray re-reads the file when it changes, so hand edits take effect on the next poll.

Change settings three ways, all through the same writer so the comments survive:

- The CLI, git-config style: `opencode-usage-tracker config plan go` sets a value, `opencode-usage-tracker config plan` reads it back, `config --list` shows everything, `config path` prints the file location, and `config --edit` opens the file in your editor.
- The tray's **Configure** menu, which prompts with kdialog or zenity when either is installed and opens the config file when neither is.
- Editing the file directly. If you set the balance by hand, set `balance_as_of` with it; the CLI and the menu stamp it for you, and the credit estimate stands on that stamp.

| Key | Values | Meaning |
| --- | --- | --- |
| `plan` | `free`, `go`, `credit` | Which resource the light follows. Default `free`. |
| `go.tier` | `go`, `go_plus`, `go_teams` | Your Go tier, whose per-model limits the Go light uses. |
| `balance` | dollars | Current Zen credit balance (credit plan). Setting it stamps `balance_as_of`; reading it prints the stamp too. |
| `balance_as_of` | UTC timestamp | When the balance was last set. Recorded spend after this moment is subtracted from the balance. |
| `balance_reminder` | `true`, `false` | On OpenCode launch, remind when the balance is unset or a day old. Default `true`. |
| `yellow_percent` | number | The credit light turns yellow when the estimated balance left falls below this percent of the balance as entered. Default 25. |
| `red_percent` | number | The same for red. Default 10. |

## Paid plans

The tracker also understands the two paid ways to use Zen. Set your plan with `opencode-usage-tracker config plan credit` or `opencode-usage-tracker config plan go`; `free` is the default.

- **Zen credit.** You enter your current balance (the tray's Configure menu, or `opencode-usage-tracker config balance 25.00`), and the tracker subtracts the recorded cost of paid requests made after the moment you entered it. The light follows the estimated balance left. Spend from other machines on the same account is not in the local database, so the estimate cannot see it. When the last 7 days include billed spend, the hover and menu add a runway line: the estimated balance left divided by that week's average daily burn.
- **OpenCode Go.** Go usage within its limits records a cost of zero, so the tracker prices each request's tokens with OpenCode's published per-model rates and compares the totals against the published window limits (5 hours at 20% of the monthly limit, 7 days at 50%, the month at 100%) for your tier.

The Go support is **untested with actual data**. The prices and limits come from OpenCode's published pages, and the arithmetic is verified against synthetic records, but the maintainer does not subscribe to Go and has never checked the figures against a live account. If you do subscribe, feedback is very welcome: an issue setting the tracker's numbers beside the Zen console's is exactly what is needed to calibrate it.

## Troubleshooting

- **No tray icon at all.** On GNOME, install and enable the extension named in [Desktop support](#desktop-support). On other desktops, check that your panel implements StatusNotifierItem.
- **The icon is gray.** The database could not be read: OpenCode has not run yet on this machine, the database is not where the tracker expects it, or it is locked. `--once` reports the same state as an error. Set `OPENCODE_DB` to point at a database in a nonstandard place.
- **The icon is missing but the app is running.** While OpenCode is closed the tray item is passive, and Plasma moves passive items into the hidden tray area. It returns when OpenCode starts.
- **The credit estimate looks wrong.** It subtracts only the spend recorded after `balance_as_of`, and it cannot see other machines on the same account. Re-enter the balance from the current Zen console figure.
- **The Go figures look wrong.** They are estimates from published prices and limits, untested against a live account. See [Paid plans](#paid-plans), and please report the difference.

## Development

```sh
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

That gate is what a change has to pass; see [CONTRIBUTING.md](CONTRIBUTING.md). CI runs `cargo build` and `cargo test` on every push and pull request to `main`. The tests cover the config file and its CLI, the tray text, the stats engine and chart, the Go price tables, and the installer, all against scratch databases.

Repository layout:

- `src/main.rs` runs the tray, counts requests, draws the icon, and defines the command line.
- `src/config.rs` holds the TOML configuration, its commented template, and the `config` CLI.
- `src/dialog.rs` is the kdialog/zenity adapter behind the Configure menu, kept free of app types so it can become its own crate.
- `src/go.rs` holds the Go price and limit tables and the rolling-window accounting.
- `src/stats.rs` implements `stats`: the query engine, and its chart, table, and JSON renderings.
- `src/install.rs` installs and uninstalls the executable and the desktop entry.
- `OCUT.desktop` is the desktop-entry template. `src/install.rs` replaces its `@EXEC@` placeholder with the installed path. The menu entry uses the standard `utilities-system-monitor` icon.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to report problems and send changes, and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for the community rules.

## License

MIT. See [LICENSE](LICENSE).
