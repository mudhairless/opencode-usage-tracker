//! `install` / `uninstall` — put the built executable and its .desktop entry
//! into the system's or the user's own directories, with optional autostart.
//!
//! Autostart is deliberately per-user: the entry lands in the config dir of
//! whoever invoked the command — the real user even when this runs under
//! `sudo`/`pkexec` — never in the global /etc/xdg/autostart, which would
//! start the tray for every login on the machine.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Executable and desktop-entry base name.
const BIN: &str = env!("CARGO_PKG_NAME");
/// Installed desktop-entry file name.
const DESKTOP_FILE: &str = concat!(env!("CARGO_PKG_NAME"), ".desktop");
/// Template kept at the repo root; `@EXEC@` becomes the absolute path of the
/// installed executable.
const TEMPLATE: &str = include_str!("../OCUT.desktop");
const EXEC_TOKEN: &str = "@EXEC@";
/// FHS home for locally built software; where `--system` installs go.
const SYSTEM_BIN_DIR: &str = "/usr/local/bin";
const SYSTEM_APPS_DIR: &str = "/usr/share/applications";
const BIN_MODE: u32 = 0o755;

// ---------------------------------------------------------------------------
// The user a command runs for
// ---------------------------------------------------------------------------

struct User {
    name: String,
    uid: Option<u32>,
    home: PathBuf,
}

struct Entry {
    name: String,
    uid: u32,
    home: PathBuf,
}

/// Our effective uid, read from /proc so this needs no libc.
fn current_euid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    // "Uid:\t<real>\t<effective>\t<saved>\t<fs>"
    line.split_whitespace().nth(1)?.parse().ok()
}

fn parse_passwd(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            // name:password:uid:gid:gecos:home:shell
            let mut f = line.split(':');
            let name = f.next()?.to_string();
            f.next()?;
            let uid = f.next()?.parse().ok()?;
            f.next()?;
            f.next()?;
            let home = f.next()?.to_string();
            if !home.starts_with('/') {
                return None;
            }
            Some(Entry {
                name,
                uid,
                home: PathBuf::from(home),
            })
        })
        .collect()
}

fn passwd_entries() -> Vec<Entry> {
    fs::read_to_string("/etc/passwd")
        .map(|text| parse_passwd(&text))
        .unwrap_or_default()
}

fn from_entry(e: &Entry) -> User {
    User {
        name: e.name.clone(),
        uid: Some(e.uid),
        home: e.home.clone(),
    }
}

/// The user to install for: whoever invoked us. Under `sudo`/`pkexec` that is
/// the person who escalated, not root — so their home gets the autostart
/// entry, not /root's.
fn invoking_user() -> Result<User> {
    let entries = passwd_entries();

    // sudo always sets SUDO_USER; pkexec (KDE's usual escalation tool) sets
    // PKEXEC_UID; failing both, we simply are ourselves.
    if let Some(name) = std::env::var("SUDO_USER")
        .ok()
        .filter(|n| !n.is_empty() && n.as_str() != "root")
        && let Some(e) = entries.iter().find(|e| e.name == name)
    {
        return Ok(from_entry(e));
    }
    if let Some(uid) = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|uid| *uid != 0)
        && let Some(e) = entries.iter().find(|e| e.uid == uid)
    {
        return Ok(from_entry(e));
    }
    if let Some(euid) = current_euid()
        && let Some(e) = entries.iter().find(|e| e.uid == euid)
    {
        return Ok(from_entry(e));
    }
    // Last resort, e.g. an NSS-managed account with no /etc/passwd line:
    // trust a valid $HOME (name and uid stay unknown).
    let home = std::env::var("HOME").unwrap_or_default();
    if home.starts_with('/') {
        return Ok(User {
            name: std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
            uid: current_euid(),
            home: PathBuf::from(home),
        });
    }
    bail!("cannot work out the home directory of the user running this command; set $HOME")
}

/// `$XDG_*_HOME` is honoured only when we are that user — under `sudo` it
/// names root's config instead of theirs. Per the XDG spec, non-absolute
/// values don't count.
fn xdg_home(var: &str, user: &User, fallback: &str) -> PathBuf {
    if user.uid.is_some()
        && user.uid == current_euid()
        && let Ok(value) = std::env::var(var)
    {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
    }
    user.home.join(fallback)
}

fn config_home(user: &User) -> PathBuf {
    xdg_home("XDG_CONFIG_HOME", user, ".config")
}

fn data_home(user: &User) -> PathBuf {
    xdg_home("XDG_DATA_HOME", user, ".local/share")
}

// ---------------------------------------------------------------------------
// Where files go
// ---------------------------------------------------------------------------

struct Target {
    bin: PathBuf,
    apps: PathBuf,
    autostart: PathBuf,
}

fn target(system: bool, user: &User) -> Target {
    let (bin_dir, apps_dir) = if system {
        (
            PathBuf::from(SYSTEM_BIN_DIR),
            PathBuf::from(SYSTEM_APPS_DIR),
        )
    } else {
        (
            user.home.join(".local/bin"),
            data_home(user).join("applications"),
        )
    };
    Target {
        bin: bin_dir.join(BIN),
        apps: apps_dir.join(DESKTOP_FILE),
        // Per-user even for a system-wide install: autostart belongs to one
        // desktop session, not to every account that logs into the machine.
        autostart: config_home(user).join("autostart").join(DESKTOP_FILE),
    }
}

/// Fail early — with the exact command to re-run — instead of writing files
/// as the wrong user. `extra` is the rest of the user's own flags.
fn check_privileges(system: bool, action: &str, extra: &str) -> Result<()> {
    let escalated = std::env::var("SUDO_USER")
        .ok()
        .filter(|n| !n.is_empty() && n.as_str() != "root")
        .is_some()
        || std::env::var_os("PKEXEC_UID").is_some();
    if system {
        if current_euid() != Some(0) {
            bail!("{action} --system needs root.\nre-run as:  sudo {BIN} {action} --system{extra}");
        }
    } else if current_euid() == Some(0) && escalated {
        bail!(
            "{action} only touches your own files — no root needed.\nre-run as:  {BIN} {action}{extra}"
        );
    }
    Ok(())
}

fn source_binary(from: Option<&Path>) -> Result<PathBuf> {
    let src = match from {
        Some(p) => p.to_path_buf(),
        None => std::env::current_exe().context("cannot locate the running executable")?,
    };
    let meta = fs::metadata(&src).with_context(|| format!("cannot read {}", src.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", src.display());
    }
    Ok(src)
}

fn render_desktop(bin_path: &Path) -> Result<String> {
    if !TEMPLATE.contains(EXEC_TOKEN) {
        bail!(
            "desktop template has no {EXEC_TOKEN} placeholder — refusing to write a broken entry"
        );
    }
    let exec = bin_path.to_string_lossy().into_owned();
    // Exec= splits on unquoted spaces, so a path containing any has to be quoted.
    let exec = if exec.chars().any(char::is_whitespace) {
        format!("\"{exec}\"")
    } else {
        exec
    };
    Ok(TEMPLATE.replace(EXEC_TOKEN, &exec))
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))
}

fn dir_in_path(dir: Option<&Path>) -> bool {
    let Some(dir) = dir else {
        return false;
    };
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':')
        .any(|entry| !entry.is_empty() && Path::new(entry) == dir)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Stage the new binary beside the old one and rename it into place. Writing
/// straight over a running executable fails with ETXTBSY; a rename works, and
/// it also replaces a stale symlink instead of following it.
fn install_binary(src: &Path, dest: &Path) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let tmp = dest.with_file_name(format!(".{BIN}.tmp.{}", std::process::id()));
    let staged = fs::copy(src, &tmp)
        .with_context(|| format!("cannot copy {} to {}", src.display(), tmp.display()))
        .and_then(|_| {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(BIN_MODE))
                .with_context(|| format!("cannot chmod {}", tmp.display()))
        });
    if let Err(e) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dest).with_context(|| format!("cannot move {} into place", dest.display()))
}

pub(crate) fn install(system: bool, autostart: bool, from: Option<&Path>) -> Result<()> {
    let extra = if autostart { " --autostart" } else { "" };
    check_privileges(system, "install", extra)?;

    let user = invoking_user()?;
    let src = source_binary(from)?;
    let target = target(system, &user);

    // Installing the very file we are running would change nothing.
    let already_there = match (fs::canonicalize(&src), fs::canonicalize(&target.bin)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if already_there {
        println!("binary already in place: {}", target.bin.display());
    } else {
        install_binary(&src, &target.bin)?;
        println!("installed binary: {}", target.bin.display());
    }

    // One rendered entry serves both the menu and autostart: Exec points at
    // the installed copy, never at the build directory.
    let desktop = render_desktop(&target.bin)?;
    write_file(&target.apps, &desktop)?;
    println!("installed menu entry: {}", target.apps.display());

    if autostart {
        write_file(&target.autostart, &desktop)?;
        println!(
            "installed autostart: {} (user: {})",
            target.autostart.display(),
            user.name
        );
    } else if fs::remove_file(&target.autostart).is_ok() {
        println!("removed autostart: {}", target.autostart.display());
    } else {
        println!("autostart off — pass --autostart to start the tray at login");
    }

    let bin_dir = target.bin.parent();
    if !system && !dir_in_path(bin_dir) {
        let dir = bin_dir.unwrap_or(Path::new(""));
        println!(
            "note: {} is not in your PATH — add it to ~/.profile and log back in",
            dir.display()
        );
    }
    if src.to_string_lossy().contains("/target/debug/") {
        println!(
            "note: a debug build was installed — `cargo build --release` gives a smaller, faster binary"
        );
    }
    Ok(())
}

pub(crate) fn uninstall(system: bool) -> Result<()> {
    check_privileges(system, "uninstall", "")?;
    let user = invoking_user()?;
    let target = target(system, &user);

    for (label, path) in [
        ("binary", &target.bin),
        ("menu entry", &target.apps),
        ("autostart entry", &target.autostart),
    ] {
        match fs::remove_file(path) {
            Ok(()) => println!("removed {label}: {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("{label} not installed: {}", path.display());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot remove {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_passwd_lines() {
        let text = "root:x:0:0::/root:/bin/bash\n\
                    mud:x:1000:1000::/home/mud:/bin/bash\n\
                    broken\n\
                    odd:x:1001:1001::relative-home:/bin/false\n";
        let entries = parse_passwd(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "root");
        assert_eq!(entries[0].uid, 0);
        assert_eq!(entries[1].name, "mud");
        assert_eq!(entries[1].home, PathBuf::from("/home/mud"));
    }

    #[test]
    fn desktop_entry_gets_the_installed_path() {
        let text =
            render_desktop(Path::new("/home/mud/.local/bin/opencode-usage-tracker")).unwrap();
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("Exec=/home/mud/.local/bin/opencode-usage-tracker\n"));
        assert!(!text.contains(EXEC_TOKEN));
        assert!(text.contains("Icon=utilities-system-monitor"));
    }

    #[test]
    fn exec_path_with_spaces_is_quoted() {
        let text = render_desktop(Path::new("/home/mud/My Dir/opencode-usage-tracker")).unwrap();
        assert!(text.contains("Exec=\"/home/mud/My Dir/opencode-usage-tracker\""));
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ocut-install-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_binary_replaces_and_sets_mode() {
        let dir = scratch_dir("replace");
        let src = dir.join("source");
        let dest = dir.join("dest");
        fs::write(&src, b"new").unwrap();
        fs::write(&dest, b"old").unwrap();

        install_binary(&src, &dest).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"new");
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, BIN_MODE);
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn install_binary_replaces_a_symlink_not_its_target() {
        let dir = scratch_dir("symlink");
        let src = dir.join("source");
        let real = dir.join("real");
        let link = dir.join("link");
        fs::write(&src, b"new").unwrap();
        fs::write(&real, b"keep").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        install_binary(&src, &link).unwrap();

        assert_eq!(fs::read(&real).unwrap(), b"keep");
        assert!(
            !fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&link).unwrap(), b"new");
        fs::remove_dir_all(&dir).unwrap();
    }
}
