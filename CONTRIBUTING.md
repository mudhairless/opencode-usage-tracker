# Contributing

Thanks for your interest. Bug reports, fixes, and documentation improvements are all welcome.

A specific request: the OpenCode Go support (the token-priced usage lights in `src/go.rs`) is built from OpenCode's published prices and limits and is verified against synthetic data, but it has never been run against a real Go subscription. If you subscribe to Go and the tracker's figures do not match what the Zen console shows, please open an issue with both sets of numbers. That feedback is the only way the estimates get calibrated.

## Reporting problems

Open an issue on GitHub. For counting bugs, the most useful report includes:

- The output of `opencode-usage-tracker --once`.
- What the Zen console or OpenCode itself showed for the same period.
- Your plan setting (`opencode-usage-tracker config plan`), with any balance figures removed.

Never attach your real OpenCode database. It holds your session history. If a maintainer needs rows to reproduce a problem, they will ask for a synthetic database that shows the same shape.

## Development setup

Requirements are the same as for building the tracker itself: Rust 1.88 or newer (the code uses edition 2024 and let-chains) and a C compiler for the bundled SQLite.

```sh
git clone https://github.com/mudhairless/opencode-usage-tracker.git
cd opencode-usage-tracker
cargo build
cargo test
```

## Before you send a change

Run the full gate and make sure it passes:

```sh
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

CI runs `cargo build` and `cargo test` on every push and pull request to `main`, but the clippy gate above is stricter than CI. A change that fails it will be sent back.

## Ground rules

These are design decisions, not preferences, and changes that break them will not be merged:

- **No network requests, ever.** The local OpenCode database is the only data source. Balance and usage figures come from the user and the local rows, never from an API.
- **Read-only database access.** The tracker runs SELECT statements only. It opens the database read-write so a pending WAL is replayed, and that is the sole reason for the write flag.
- **Tests use scratch databases.** Tests build throwaway SQLite files with known rows (see the fixtures in `src/stats.rs` and `src/go.rs`). Never point a test at a real database.
- **The config file is its own documentation.** Every option lives in the commented template in `src/config.rs`, and programmatic writes go through the one `toml_edit`-based writer so the comments survive. A new option needs its template comment in the same change.
- **Data tables carry their provenance.** The Go prices and limits in `src/go.rs` were captured from OpenCode's published pages on the date the file records. If you update them, update the capture date and link the source in your pull request.

## Style

- Comment the *why*, not the *what*. The codebase explains its non-obvious decisions (UTC day boundaries, the unfinished-row rule, the estimate caveats) at the point where they are made. Keep that habit.
- Keep changes single-purpose. A bug fix and a refactor in one pull request will be asked to split.
- Plain language in documentation and commit messages. Short sentences, concrete nouns, no filler.

## Pull requests

Fork, branch, and open the pull request against `main`. Describe what the change does and why, and say how you verified it: the gate output, a `--once` or `stats` run against a scratch database, or a live tray check. Small, well-described changes get reviewed fastest.

By contributing, you agree that your contributions are licensed under the MIT License, and you accept the [Code of Conduct](CODE_OF_CONDUCT.md).
