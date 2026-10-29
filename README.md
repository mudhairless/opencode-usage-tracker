# opencode-usage-tracker

[![Rust](https://github.com/mudhairless/opencode-usage-tracker/actions/workflows/rust.yml/badge.svg)](https://github.com/mudhairless/opencode-usage-tracker/actions/workflows/rust.yml)

A KDE Plasma tray icon that shows how many OpenCode Zen free-tier requests you have made today. It reads OpenCode's local SQLite database every 30 seconds and colors the icon to match:

| Icon color | Requests today |
| --- | --- |
| Green | fewer than 400 |
| Yellow | 400 to 499 |
| Red | 500 or more |
| Gray | the database could not be read |

The Zen day resets at 00:00 UTC. The tooltip and the tray menu show the reset time in your local timezone.

The tracker runs no INSERT, UPDATE, or DELETE statements, and it makes no network requests. Every number comes from the local OpenCode database.

The `stats` subcommand charts the same counts per UTC day in the terminal.

## Requirements

- Linux with a system tray that implements StatusNotifierItem. Development targets KDE Plasma.
- Rust 1.88 or newer, because the code uses edition 2024 and let-chains.
- A C compiler, because the bundled SQLite in `rusqlite` builds from source.
- OpenCode used on the same machine, so that its database exists.

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
| `opencode-usage-tracker stats` | Chart free-tier usage per UTC day in the terminal. |
| `opencode-usage-tracker uninstall` | Remove an install for the current user. |

`--once` prints the same lines as the tooltip:

```
$ opencode-usage-tracker --once
OpenCode Zen Free Tier Usage
250 requests today
43 requests last hour
Resets at 7:00 PM
```

If the database cannot be read, `--once` prints `usage unavailable (database unreadable)` and exits with status 1.

### The tray item

- Hovering shows today's count, the last hour's count, and the next reset time.
- Clicking the icon refreshes the counts.
- The menu holds a summary line, **Refresh now**, and **Quit**.
- While OpenCode is not running, the icon is green and Plasma moves the item to the hidden tray area. The tooltip adds an `OpenCode not running` line.

### The stats chart

`stats` buckets the same rows into UTC days, one bar per day. Bars are eight columns wide with a one column gap, so the default day count is whichever whole number of bars fits across the terminal. Each bar prints its count at its base, tokens as `47.21M` and request counts as whole numbers, and a box in the top-right corner names what the bars count.

| Key | Effect |
| --- | --- |
| `←` and `→`, or `h` and `l` | Slide the window one day. Works only when more days are loaded than fit on screen. |
| `Tab` | Switch between tokens and requests. |
| `b` | Switch between bars and a line. |
| `q` or `Esc` | Quit. |

### stats options

| Option | Effect |
| --- | --- |
| `--days N` | How many days back to report, 1 to 365. Defaults to the most days whose bars fit across the terminal, and to 7 with `--json`. |
| `--json` | Print the per-day rows and totals as JSON instead of the chart. |

`--json` prints one object holding the days, a UTC timestamp, and the totals:

```
$ opencode-usage-tracker stats --days 2 --json
{"days":[{"date":"2026-10-18","requests":525,"tokens":44405942},{"date":"2026-10-19","requests":434,"tokens":24847216}],"generated_at_utc":"2026-10-19T04:40:51Z","totals":{"requests":959,"tokens":69253158}}
```

## Counting

The tracker counts rows in `$HOME/.local/share/opencode/opencode.db`, table `session_message`. A row counts when all of these hold:

- The row was created after 00:00 UTC today.
- The type is `assistant` or `compaction`.
- The model provider is `opencode`.
- The model is free: the id is `big-pickle`, or the id ends in `-free`.
- For `assistant` rows only, the message finished. Rows that are still streaming or were aborted do not count.

The tracker opens the database read-write so that a pending WAL is replayed, then runs only SELECT statements. Set `OPENCODE_DB` to count from a different file.

## Paid plans

The tracker also understands the two paid ways to use Zen. Set your plan with `opencode-usage-tracker config plan credit` or `opencode-usage-tracker config plan go`; `free` is the default.

- **Zen credit.** You enter your current balance (the tray's Configure menu, or `opencode-usage-tracker config balance 25.00`), and the tracker subtracts the recorded cost of paid requests made after the moment you entered it. The light follows the estimated balance left. Spend from other machines on the same account is not in the local database, so the estimate cannot see it.
- **OpenCode Go.** Go usage within its limits records a cost of zero, so the tracker prices each request's tokens with OpenCode's published per-model rates and compares the totals against the published window limits (5 hours at 20% of the monthly limit, 7 days at 50%, the month at 100%) for your tier.

The Go support is **untested with actual data**. The prices and limits come from OpenCode's published pages, and the arithmetic is verified against synthetic records, but the maintainer does not subscribe to Go and has never checked the figures against a live account. If you do subscribe, feedback is very welcome: an issue setting the tracker's numbers beside the Zen console's is exactly what is needed to calibrate it.

## Development

```sh
cargo test
cargo clippy --all-targets
```

`cargo test` covers the installer, the desktop-entry rendering, the tooltip text, and the stats chart (day bucketing and bar layout). CI runs `cargo build` and `cargo test` on every push and pull request to `main`.

Repository layout:

- `src/main.rs` counts requests, draws the icon, and defines the command line.
- `src/stats.rs` implements `stats`: the per-day query, the JSON output, and the terminal chart.
- `src/install.rs` installs and uninstalls the executable and the desktop entry.
- `OCUT.desktop` is the desktop-entry template. `src/install.rs` replaces its `@EXEC@` placeholder with the installed path. The menu entry uses the standard `utilities-system-monitor` icon.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to report problems and send changes, and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for the community rules.

## License

MIT. See [LICENSE](LICENSE).
