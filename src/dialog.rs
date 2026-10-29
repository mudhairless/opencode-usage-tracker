//! Minimal desktop dialogs over `kdialog` or `zenity`.
//!
//! This module is deliberately self-contained: it uses only the standard
//! library, knows nothing about the rest of the application, and can be
//! lifted into another project unchanged.
//!
//! Every call blocks until the user answers, so callers should run them on a
//! worker thread rather than on a UI or event-loop thread.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Which helper program a [`Dialogs`] handle drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Kdialog,
    Zenity,
}

/// Everything that can go wrong while showing a dialog.
#[derive(Debug)]
pub enum Error {
    /// Neither `kdialog` nor `zenity` was found.
    NoBackend,
    /// The helper could not be started.
    Spawn {
        program: PathBuf,
        source: std::io::Error,
    },
    /// The helper exited with a status that does not mean success or cancel.
    Exit {
        program: PathBuf,
        status: ExitStatus,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoBackend => {
                write!(f, "no dialog backend available (install kdialog or zenity)")
            }
            Error::Spawn { program, source } => {
                write!(f, "could not run {}: {source}", program.display())
            }
            Error::Exit { program, status } => {
                write!(f, "{} exited with {status}", program.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One request, independent of which backend will render it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request<'a> {
    Text {
        title: &'a str,
        label: &'a str,
        initial: Option<&'a str>,
    },
    Choice {
        title: &'a str,
        label: &'a str,
        choices: &'a [&'a str],
        current: Option<&'a str>,
    },
    Error {
        title: &'a str,
        message: &'a str,
    },
}

/// The result of interpreting a helper's exit status and stdout.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Exit 0; the entered/selected text, if any.
    Answered(Option<String>),
    /// Exit 1 with no stdout: the user cancelled.
    Cancelled,
    /// Any other exit status, or exit 1 that still printed something.
    Failed,
}

/// Build the argument list for `backend`, excluding argv[0].
fn argv(backend: Backend, request: &Request<'_>) -> Vec<String> {
    match *request {
        Request::Text {
            title,
            label,
            initial,
        } => match backend {
            Backend::Kdialog => {
                let mut args = vec![
                    "--title".to_string(),
                    title.to_string(),
                    "--inputbox".to_string(),
                    label.to_string(),
                ];
                if let Some(initial) = initial {
                    args.push(initial.to_string());
                }
                args
            }
            Backend::Zenity => {
                let mut args = vec![
                    "--entry".to_string(),
                    "--title".to_string(),
                    title.to_string(),
                    "--text".to_string(),
                    label.to_string(),
                ];
                if let Some(initial) = initial {
                    args.push("--entry-text".to_string());
                    args.push(initial.to_string());
                }
                args
            }
        },
        Request::Choice {
            title,
            label,
            choices,
            current,
        } => {
            let selected = current
                .filter(|current| choices.contains(current))
                .or_else(|| choices.first().copied());

            match backend {
                Backend::Kdialog => {
                    let mut args = vec![
                        "--title".to_string(),
                        title.to_string(),
                        "--radiolist".to_string(),
                        label.to_string(),
                    ];
                    for choice in choices {
                        args.push((*choice).to_string());
                        args.push((*choice).to_string());
                        args.push(
                            if Some(*choice) == selected {
                                "on"
                            } else {
                                "off"
                            }
                            .to_string(),
                        );
                    }
                    args
                }
                Backend::Zenity => {
                    let mut args = vec![
                        "--list".to_string(),
                        "--radiolist".to_string(),
                        "--title".to_string(),
                        title.to_string(),
                        "--text".to_string(),
                        label.to_string(),
                        "--column".to_string(),
                        "pick".to_string(),
                        "--column".to_string(),
                        "option".to_string(),
                    ];
                    for choice in choices {
                        args.push(
                            if Some(*choice) == selected {
                                "TRUE"
                            } else {
                                "FALSE"
                            }
                            .to_string(),
                        );
                        args.push((*choice).to_string());
                    }
                    args
                }
            }
        }
        Request::Error { title, message } => match backend {
            Backend::Kdialog => vec![
                "--title".to_string(),
                title.to_string(),
                "--error".to_string(),
                message.to_string(),
            ],
            Backend::Zenity => vec![
                "--title".to_string(),
                title.to_string(),
                "--error".to_string(),
                "--text".to_string(),
                message.to_string(),
            ],
        },
    }
}

/// Interpret an exit code and stdout without touching the OS.
fn classify(exit_code: Option<i32>, stdout: &str) -> Outcome {
    match exit_code {
        Some(0) => {
            let text = stdout.trim_end_matches(['\r', '\n']);
            if text.trim().is_empty() {
                Outcome::Answered(None)
            } else {
                Outcome::Answered(Some(text.to_string()))
            }
        }
        Some(1) if stdout.trim().is_empty() => Outcome::Cancelled,
        _ => Outcome::Failed,
    }
}

/// A handle to the best dialog backend found on this machine.
#[derive(Clone, Debug)]
pub struct Dialogs {
    backend: Option<(Backend, PathBuf)>,
}

impl Dialogs {
    /// Probe `PATH`, preferring `kdialog` over `zenity`.
    pub fn new() -> Dialogs {
        Dialogs::with_paths(find_on_path("kdialog"), find_on_path("zenity"))
    }

    /// Build a handle from explicit executable paths, bypassing `PATH`.
    /// Primarily useful for tests.
    pub fn with_paths(kdialog: Option<PathBuf>, zenity: Option<PathBuf>) -> Dialogs {
        let backend = kdialog
            .map(|path| (Backend::Kdialog, path))
            .or_else(|| zenity.map(|path| (Backend::Zenity, path)));
        Dialogs { backend }
    }

    /// Is there a backend to drive?
    pub fn available(&self) -> bool {
        self.backend.is_some()
    }

    /// Ask for a line of text. `Ok(None)` means the user cancelled.
    pub fn prompt_text(
        &self,
        title: &str,
        label: &str,
        initial: Option<&str>,
    ) -> Result<Option<String>, Error> {
        self.run(Request::Text {
            title,
            label,
            initial,
        })
    }

    /// Ask the user to pick one of `choices`. `Ok(None)` means they cancelled.
    #[allow(dead_code)] // part of the liftable adapter API; the tray has no picker yet
    pub fn prompt_choice(
        &self,
        title: &str,
        label: &str,
        choices: &[&str],
        current: Option<&str>,
    ) -> Result<Option<String>, Error> {
        self.run(Request::Choice {
            title,
            label,
            choices,
            current,
        })
    }

    /// Show a message with a single dismiss button.
    pub fn show_error(&self, title: &str, message: &str) -> Result<(), Error> {
        self.run(Request::Error { title, message }).map(|_| ())
    }

    fn run(&self, request: Request<'_>) -> Result<Option<String>, Error> {
        let Some((backend, program)) = &self.backend else {
            return Err(Error::NoBackend);
        };

        let output = Command::new(program)
            .args(argv(*backend, &request))
            .stdin(Stdio::null())
            .output()
            .map_err(|source| Error::Spawn {
                program: program.clone(),
                source,
            })?;

        match classify(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
        ) {
            Outcome::Answered(text) => Ok(text),
            Outcome::Cancelled => Ok(None),
            Outcome::Failed => Err(Error::Exit {
                program: program.clone(),
                status: output.status,
            }),
        }
    }
}

impl Default for Dialogs {
    fn default() -> Self {
        Dialogs::new()
    }
}

/// Find an executable on `PATH`.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn kdialog_text_argv() {
        let with_initial = Request::Text {
            title: "Title",
            label: "Label",
            initial: Some("1.00"),
        };
        assert_eq!(
            argv(Backend::Kdialog, &with_initial),
            args(&["--title", "Title", "--inputbox", "Label", "1.00"])
        );

        let without_initial = Request::Text {
            title: "Title",
            label: "Label",
            initial: None,
        };
        assert_eq!(
            argv(Backend::Kdialog, &without_initial),
            args(&["--title", "Title", "--inputbox", "Label"])
        );
    }

    #[test]
    fn zenity_text_argv() {
        let with_initial = Request::Text {
            title: "Title",
            label: "Label",
            initial: Some("1.00"),
        };
        assert_eq!(
            argv(Backend::Zenity, &with_initial),
            args(&[
                "--entry",
                "--title",
                "Title",
                "--text",
                "Label",
                "--entry-text",
                "1.00",
            ])
        );

        let without_initial = Request::Text {
            title: "Title",
            label: "Label",
            initial: None,
        };
        assert_eq!(
            argv(Backend::Zenity, &without_initial),
            args(&["--entry", "--title", "Title", "--text", "Label"])
        );
    }

    #[test]
    fn kdialog_choice_argv_marks_the_current_choice() {
        let request = Request::Choice {
            title: "Title",
            label: "Label",
            choices: &["a", "b"],
            current: Some("b"),
        };
        assert_eq!(
            argv(Backend::Kdialog, &request),
            args(&[
                "--title",
                "Title",
                "--radiolist",
                "Label",
                "a",
                "a",
                "off",
                "b",
                "b",
                "on",
            ])
        );
    }

    #[test]
    fn choice_falls_back_to_the_first_entry() {
        let request = Request::Choice {
            title: "Title",
            label: "Label",
            choices: &["a", "b"],
            current: Some("missing"),
        };
        assert_eq!(
            argv(Backend::Zenity, &request),
            args(&[
                "--list",
                "--radiolist",
                "--title",
                "Title",
                "--text",
                "Label",
                "--column",
                "pick",
                "--column",
                "option",
                "TRUE",
                "a",
                "FALSE",
                "b",
            ])
        );
    }

    #[test]
    fn error_argv() {
        let request = Request::Error {
            title: "Title",
            message: "Message",
        };
        assert_eq!(
            argv(Backend::Kdialog, &request),
            args(&["--title", "Title", "--error", "Message"])
        );
        assert_eq!(
            argv(Backend::Zenity, &request),
            args(&["--title", "Title", "--error", "--text", "Message"])
        );
    }

    #[test]
    fn classifier_reads_success_cancel_and_failure() {
        assert_eq!(
            classify(Some(0), "12.50\n"),
            Outcome::Answered(Some("12.50".to_string()))
        );
        assert_eq!(classify(Some(0), ""), Outcome::Answered(None));
        assert_eq!(classify(Some(1), ""), Outcome::Cancelled);
        assert_eq!(classify(Some(1), "still text"), Outcome::Failed);
        assert_eq!(classify(Some(2), ""), Outcome::Failed);
        assert_eq!(classify(None, ""), Outcome::Failed);
    }

    #[test]
    fn explicit_paths_choose_the_backend() {
        assert!(!Dialogs::with_paths(None, None).available());
        assert!(Dialogs::with_paths(Some(PathBuf::from("/bin/kdialog")), None).available());
        assert!(Dialogs::with_paths(None, Some(PathBuf::from("/bin/zenity"))).available());
    }
}
