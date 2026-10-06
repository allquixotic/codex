//! Operating-system integration that Slint and winit do not provide.
//!
//! - macOS: Cmd+Q, Dock › Quit and logout send `terminate:` to the
//!   application, which would exit the process without the app's quit
//!   confirmation or shutdown ([`install_termination_handler`]).
//! - macOS: apps started from Finder or the Dock inherit launchd's minimal
//!   `PATH`; [`import_login_shell_path`] adopts the login shell's.
//! - macOS: [`main_bundle_identifier`] lets notifications name the app.
//! - Windows: the GUI subsystem has no console for `--help` and argument
//!   errors ([`attach_parent_console`]).
//! - Linux: [`opengl_available`] lets the default renderer skip OpenGL on
//!   machines without GL libraries.

/// Windows remote desktops can advertise OpenGL while losing the presentation
/// surface across reconnects. Use CPU rendering for Auto in those sessions.
pub(crate) fn remote_desktop_session() -> bool {
    #[cfg(windows)]
    {
        // SAFETY: reads one system metric; no pointers or owned resources.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows_sys::Win32::UI::WindowsAndMessaging::SM_REMOTESESSION,
            ) != 0
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Asking a login shell for its `PATH` (macOS; tested on every Unix).
#[cfg(any(target_os = "macos", all(test, unix)))]
mod login_shell {
    use std::path::Path;
    use std::time::Duration;
    use std::time::Instant;

    pub(super) const PATH_MARKER: &str = "__CODEX_GUI_PATH__";

    /// Shells whose `-l -c` flags we know.
    pub(super) fn supports_login_flags(shell: &Path) -> bool {
        let name = shell
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        matches!(name, "zsh" | "bash" | "fish" | "sh" | "ksh" | "dash")
    }

    /// Text between the first two markers, without the trailing newline
    /// that `printenv` adds. Shell start-up files may print around it.
    pub(super) fn parse_marked_path(output: &str) -> Option<String> {
        let start = output.find(PATH_MARKER)? + PATH_MARKER.len();
        let rest = &output[start..];
        let end = rest.find(PATH_MARKER)?;
        let path = rest[..end].trim_end_matches(['\r', '\n']);
        (!path.trim().is_empty()).then(|| path.to_string())
    }

    /// `shell_path` followed by the entries of `current` it lacks, so
    /// nothing the process could already find goes missing.
    pub(super) fn merge_paths(shell_path: &str, current: &str) -> String {
        let mut entries: Vec<&str> = shell_path
            .split(':')
            .filter(|entry| !entry.is_empty())
            .collect();
        for entry in current.split(':') {
            if !entry.is_empty() && !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        entries.join(":")
    }

    /// Runs `shell` with `args` to print its `PATH`, killing it after
    /// `timeout`. Returns `None` on any failure.
    pub(super) fn login_shell_path(
        shell: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Option<String> {
        use std::io::Read;
        use std::process::Command;
        use std::process::Stdio;

        let command =
            format!("printf '%s' {PATH_MARKER}; /usr/bin/printenv PATH; printf '%s' {PATH_MARKER}");
        let mut child = Command::new(shell)
            .args(args)
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    // No threads may exist yet (the caller then sets PATH),
                    // so poll instead of waiting on a helper thread.
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let mut output = String::new();
        child.stdout.take()?.read_to_string(&mut output).ok()?;
        parse_marked_path(&output)
    }
}

/// Upper bound for asking the login shell for its `PATH`.
#[cfg(target_os = "macos")]
const LOGIN_SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// On macOS, when the app was not started from a terminal, replaces the
/// minimal `PATH` launchd gives Finder and Dock launches with the user's
/// login-shell `PATH`, so MCP servers, hooks and other tools that are
/// started without a shell (`npx`, `uvx`, `docker`, ...) are found.
///
/// The shell runs as a login shell but not an interactive one: it reads
/// `/etc/zprofile` (`path_helper`), `~/.zprofile`, `~/.zshenv` and
/// `~/.zlogin` (bash: `~/.bash_profile` or `~/.profile`), not `~/.zshrc`.
/// Interactive start-up files can block for good when an app without
/// folder permissions runs them (completion setup touching protected
/// folders waits on a privacy prompt), which would stall every launch.
///
/// # Safety
///
/// Calls `std::env::set_var`; the process must still be single-threaded.
#[cfg(target_os = "macos")]
pub(crate) unsafe fn import_login_shell_path() {
    use std::io::IsTerminal;

    if std::env::var_os("TERM").is_some() || std::io::stdin().is_terminal() {
        return;
    }
    let shell = std::env::var_os("SHELL")
        .map(std::path::PathBuf::from)
        .filter(|shell| shell.is_absolute() && login_shell::supports_login_flags(shell))
        .unwrap_or_else(|| std::path::PathBuf::from("/bin/zsh"));
    let Some(shell_path) =
        login_shell::login_shell_path(&shell, &["-l", "-c"], LOGIN_SHELL_TIMEOUT)
    else {
        crate::perf::mark("login-shell-path-unavailable");
        return;
    };
    crate::perf::mark("login-shell-path-read");
    let current = std::env::var("PATH").unwrap_or_default();
    let merged = login_shell::merge_paths(&shell_path, &current);
    if merged != current {
        // SAFETY: the caller guarantees no other thread exists yet.
        unsafe {
            std::env::set_var("PATH", merged);
        }
    }
}

/// Attaches to the console of the process that started us (cmd.exe or
/// PowerShell), so `--help` and argument errors print there. Returns false
/// when there is none, for example when started from a shortcut.
#[cfg(windows)]
pub(crate) fn attach_parent_console() -> bool {
    use windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS;
    use windows_sys::Win32::System::Console::AttachConsole;

    // SAFETY: plain Win32 call without pointers.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}

/// Whether an OpenGL (EGL or GLX) client library can be loaded. The
/// OpenGL renderer only finds out when the window is shown, which is too
/// late to pick another renderer in this process.
#[cfg(target_os = "linux")]
pub(crate) fn opengl_available() -> bool {
    ["libEGL.so.1", "libGL.so.1"].iter().any(|name| {
        let Ok(name) = std::ffi::CString::new(*name) else {
            return false;
        };
        // SAFETY: dlopen with a valid C string; the handle is intentionally
        // kept open because the renderer loads the same library next.
        !unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) }.is_null()
    })
}

#[cfg(target_os = "macos")]
pub(crate) use macos::install_termination_handler;
#[cfg(target_os = "macos")]
pub(crate) use macos::main_bundle_identifier;
#[cfg(target_os = "macos")]
pub(crate) use macos::request_app_termination;

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;

    use objc2::class;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyClass;
    use objc2::runtime::AnyObject;
    use objc2::runtime::Imp;
    use objc2::runtime::Sel;
    use objc2::sel;

    /// `NSApplicationTerminateReply`.
    const NS_TERMINATE_CANCEL: usize = 0;
    const NS_TERMINATE_NOW: usize = 1;
    /// `keyAEQuitReason` ('why?'), set on the quit Apple Event that logout,
    /// restart and shutdown send.
    const KEY_AE_QUIT_REASON: u32 = u32::from_be_bytes(*b"why?");

    thread_local! {
        static HOOKS: RefCell<Option<Hooks>> = const { RefCell::new(None) };
    }

    struct Hooks {
        /// Runs the normal, confirmable quit (Cmd+Q, Dock › Quit).
        request_quit: Box<dyn Fn()>,
        /// Shuts down synchronously when the session ends (logout).
        session_end: Option<Box<dyn FnOnce()>>,
    }

    /// Routes `applicationShouldTerminate:` to the app.
    ///
    /// winit's application delegate does not implement it, so AppKit would
    /// terminate right away. The handler cancels the termination and calls
    /// `request_quit`, which asks for confirmation when turns are running
    /// and then leaves the event loop so the normal shutdown runs. When the
    /// user logs out (or restarts or shuts down the Mac), cancelling would
    /// abort the logout, so `session_end` runs instead and the app quits.
    ///
    /// Must be called on the main thread after the event loop exists.
    pub(crate) fn install_termination_handler(
        request_quit: impl Fn() + 'static,
        session_end: impl FnOnce() + 'static,
    ) {
        HOOKS.with(|hooks| {
            *hooks.borrow_mut() = Some(Hooks {
                request_quit: Box::new(request_quit),
                session_end: Some(Box::new(session_end)),
            });
        });
        // SAFETY: standard AppKit messages on the main thread; the added
        // method's signature matches `applicationShouldTerminate:`
        // (`NSApplicationTerminateReply (NSApplication *)`).
        unsafe {
            let app: Option<Retained<AnyObject>> =
                msg_send![class!(NSApplication), sharedApplication];
            let Some(app) = app else {
                tracing::warn!("NSApp is missing; Cmd+Q will skip the quit confirmation");
                return;
            };
            let delegate: Option<Retained<AnyObject>> = msg_send![&*app, delegate];
            let Some(delegate) = delegate else {
                tracing::warn!("NSApp has no delegate; Cmd+Q will skip the quit confirmation");
                return;
            };
            let class: *const AnyClass = delegate.class();
            let imp: Imp = std::mem::transmute::<
                extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject) -> usize,
                Imp,
            >(should_terminate);
            let added = objc2::ffi::class_addMethod(
                class.cast_mut(),
                sel!(applicationShouldTerminate:),
                imp,
                c"Q@:@".as_ptr(),
            );
            if !added.as_bool() {
                tracing::warn!("could not add applicationShouldTerminate: to the app delegate");
                return;
            }
            // AppKit may cache which optional methods a delegate implements
            // when it is assigned; assign it again so the new one is seen.
            let none: Option<&AnyObject> = None;
            let _: () = msg_send![&*app, setDelegate: none];
            let _: () = msg_send![&*app, setDelegate: Some(&*delegate)];
        }
    }

    /// `CFBundleIdentifier` of the running app bundle; `None` for a bare
    /// executable.
    pub(crate) fn main_bundle_identifier() -> Option<String> {
        // SAFETY: Foundation messages; every object may be nil and is
        // checked, and the C string is copied before the NSString can go.
        unsafe {
            let bundle: Option<Retained<AnyObject>> = msg_send![class!(NSBundle), mainBundle];
            let identifier: Option<Retained<AnyObject>> = msg_send![&*bundle?, bundleIdentifier];
            let identifier = identifier?;
            let utf8: *const std::ffi::c_char = msg_send![&*identifier, UTF8String];
            if utf8.is_null() {
                return None;
            }
            let identifier = std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned();
            (!identifier.is_empty()).then_some(identifier)
        }
    }

    /// Sends `terminate:` to the application, as Cmd+Q does (automation).
    pub(crate) fn request_app_termination() {
        // SAFETY: standard AppKit message on the main thread.
        unsafe {
            let app: Option<Retained<AnyObject>> =
                msg_send![class!(NSApplication), sharedApplication];
            if let Some(app) = app {
                let sender: Option<&AnyObject> = None;
                let _: () = msg_send![&*app, terminate: sender];
            }
        }
    }

    extern "C-unwind" fn should_terminate(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: *mut AnyObject,
    ) -> usize {
        if session_is_ending() {
            let session_end = HOOKS.with(|hooks| {
                hooks
                    .try_borrow_mut()
                    .ok()
                    .and_then(|mut hooks| hooks.as_mut()?.session_end.take())
            });
            if let Some(session_end) = session_end {
                session_end();
            }
            return NS_TERMINATE_NOW;
        }
        let handled = HOOKS.with(|hooks| match hooks.try_borrow() {
            Ok(hooks) => match hooks.as_ref() {
                Some(hooks) => {
                    (hooks.request_quit)();
                    true
                }
                None => false,
            },
            Err(_) => false,
        });
        if handled {
            NS_TERMINATE_CANCEL
        } else {
            NS_TERMINATE_NOW
        }
    }

    /// Whether the quit request comes from logout, restart or shutdown.
    fn session_is_ending() -> bool {
        // SAFETY: Foundation messages on the main thread inside an AppKit
        // callback; every object may be nil and is checked.
        unsafe {
            let manager: Option<Retained<AnyObject>> =
                msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            let Some(manager) = manager else {
                return false;
            };
            let event: Option<Retained<AnyObject>> = msg_send![&*manager, currentAppleEvent];
            let Some(event) = event else {
                return false;
            };
            let reason: Option<Retained<AnyObject>> =
                msg_send![&*event, attributeDescriptorForKeyword: KEY_AE_QUIT_REASON];
            let Some(reason) = reason else {
                return false;
            };
            let code: u32 = msg_send![&*reason, enumCodeValue];
            code != 0
        }
    }
}

// Everything tested here is Unix-only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use login_shell::*;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn marked_path_ignores_start_up_noise() {
        let output =
            format!("Welcome!\n{PATH_MARKER}/opt/homebrew/bin:/usr/bin\n{PATH_MARKER}bye\n");
        assert_eq!(
            parse_marked_path(&output),
            Some("/opt/homebrew/bin:/usr/bin".to_string())
        );
        assert_eq!(parse_marked_path("no markers"), None);
        assert_eq!(
            parse_marked_path(&format!("{PATH_MARKER}\n{PATH_MARKER}")),
            None
        );
    }

    #[test]
    fn merged_path_keeps_shell_order_and_current_entries() {
        assert_eq!(
            merge_paths(
                "/opt/homebrew/bin:/usr/bin:/bin",
                "/usr/bin:/bin:/usr/sbin:/sbin"
            ),
            "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert_eq!(merge_paths("", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn only_known_shells_get_login_flags() {
        assert!(supports_login_flags(Path::new("/bin/zsh")));
        assert!(supports_login_flags(Path::new("/opt/homebrew/bin/fish")));
        assert!(!supports_login_flags(Path::new("/bin/tcsh")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bare_executables_have_no_bundle_identifier() {
        // The test binary is not inside an app bundle, so notifications keep
        // notify-rust's default attribution.
        assert_eq!(main_bundle_identifier(), None);
    }

    #[test]
    fn login_shell_path_reads_the_shell_environment() {
        let path = login_shell_path(Path::new("/bin/sh"), &["-c"], Duration::from_secs(10));
        assert_eq!(
            path,
            std::env::var("PATH").ok().filter(|path| !path.is_empty())
        );
    }

    #[test]
    fn login_shell_path_gives_up_after_the_timeout() {
        // `-c` runs the script; the extra command argument becomes `$0`.
        let started = Instant::now();
        let path = login_shell_path(
            Path::new("/bin/sh"),
            &["-c", "sleep 5"],
            Duration::from_millis(100),
        );
        assert_eq!(path, None);
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
