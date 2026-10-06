//! Opening a file with its default app and revealing it in the system file
//! manager.
//!
//! "Opening" a program runs it: the launchers start executables, scripts
//! and shortcuts instead of showing them. Files the agent wrote carry no
//! quarantine mark, so nothing else would ask first; [`is_executable`]
//! lets the viewer ask.

use std::path::Path;
use std::process::Command;
use std::process::Stdio;

/// Extensions the system launchers run (or hand to an interpreter) rather
/// than show, on any of the platforms.
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    // macOS
    "app",
    "command",
    "tool",
    "terminal",
    "workflow",
    "action",
    "scpt",
    "applescript",
    "pkg",
    "mpkg",
    // Windows
    "exe",
    "com",
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "msi",
    "msp",
    "scr",
    "pif",
    "cpl",
    "hta",
    "lnk",
    "url",
    "reg",
    "inf",
    "msc",
    "application",
    "appref-ms",
    "py",
    "pyw",
    // Linux and Unix
    "desktop",
    "sh",
    "bash",
    "zsh",
    "csh",
    "ksh",
    "run",
    "appimage",
    // Any platform with a Java runtime
    "jar",
];

/// Whether the launcher would run `path` rather than show it, judging by
/// its extension.
pub(super) fn has_executable_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXECUTABLE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// Whether `path` (with `metadata`) can run as a program: an executable
/// extension, or (on Unix) an execute permission bit.
pub(super) fn is_executable(path: &Path, metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return true;
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    has_executable_extension(path)
}

/// [`is_executable`] for the file at `path` now (blocking).
pub(super) fn is_executable_now(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => is_executable(path, &metadata),
        Err(_) => has_executable_extension(path),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalAction {
    /// Open with the default app for the file type.
    Open,
    /// Show the file in Finder / Explorer / the file manager.
    Reveal,
}

/// Runs the platform launcher for `action`, falling back to the default
/// browser when no launcher is available. Waits for the launcher to exit,
/// so call it off the UI thread.
pub(crate) fn run(action: ExternalAction, path: &Path) -> Result<(), String> {
    let mut command = command_for(action, path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.status() {
        // explorer.exe reports a failure exit code even when it succeeds.
        Ok(status) if status.success() || cfg!(windows) => Ok(()),
        Ok(status) => Err(format!("Could not open {}: {status}", path.display())),
        Err(err) => {
            tracing::warn!(%err, "file launcher unavailable; falling back to the browser");
            let target = match action {
                ExternalAction::Open => path,
                ExternalAction::Reveal => path.parent().unwrap_or(path),
            };
            webbrowser::open(&target.to_string_lossy())
                .map_err(|err| format!("Could not open {}: {err}", target.display()))
        }
    }
}

fn command_for(action: ExternalAction, path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        if action == ExternalAction::Reveal {
            command.arg("-R");
        }
        command.arg(path);
        command
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("explorer");
        match action {
            ExternalAction::Open => {
                command.arg(path);
            }
            ExternalAction::Reveal => {
                // `/select,` must be followed by the quoted path verbatim.
                command.raw_arg(format!("/select,\"{}\"", path.display()));
            }
        }
        command
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let mut command = Command::new("xdg-open");
        match action {
            ExternalAction::Open => command.arg(path),
            ExternalAction::Reveal => command.arg(path.parent().unwrap_or(path)),
        };
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsStr;

    fn argv(command: &Command) -> Vec<&OsStr> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .collect()
    }

    #[test]
    fn programs_are_recognized_by_extension() {
        for program in [
            "run.command",
            "x.EXE",
            "setup.bat",
            "a.desktop",
            "t.jar",
            "s.ps1",
        ] {
            assert!(has_executable_extension(Path::new(program)), "{program}");
        }
        for document in [
            "notes.md",
            "image.png",
            "lib.rs",
            "Makefile",
            "archive.tar.gz",
        ] {
            assert!(!has_executable_extension(Path::new(document)), "{document}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn execute_bits_mark_programs() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        // An extensionless binary or script, as an agent could write one.
        let tool = dir.path().join("tool");
        std::fs::write(&tool, b"#!/bin/sh\necho hi\n")?;
        assert!(!is_executable_now(&tool));
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))?;
        assert!(is_executable_now(&tool));
        // Folders have execute bits too.
        let metadata = std::fs::metadata(dir.path())?;
        assert!(!is_executable(dir.path(), &metadata));
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_commands() {
        let path = Path::new("/tmp/a b.txt");
        assert_eq!(
            argv(&command_for(ExternalAction::Open, path)),
            vec![OsStr::new("open"), OsStr::new("/tmp/a b.txt")]
        );
        assert_eq!(
            argv(&command_for(ExternalAction::Reveal, path)),
            vec![
                OsStr::new("open"),
                OsStr::new("-R"),
                OsStr::new("/tmp/a b.txt")
            ]
        );
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn xdg_commands() {
        let path = Path::new("/tmp/dir/a.txt");
        assert_eq!(
            argv(&command_for(ExternalAction::Open, path)),
            vec![OsStr::new("xdg-open"), OsStr::new("/tmp/dir/a.txt")]
        );
        assert_eq!(
            argv(&command_for(ExternalAction::Reveal, path)),
            vec![OsStr::new("xdg-open"), OsStr::new("/tmp/dir")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_commands() {
        let path = Path::new(r"C:\work\a.txt");
        assert_eq!(
            argv(&command_for(ExternalAction::Open, path)),
            vec![OsStr::new("explorer"), OsStr::new(r"C:\work\a.txt")]
        );
    }
}
