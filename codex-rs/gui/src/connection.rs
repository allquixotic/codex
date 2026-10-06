//! Where the GUI's app-server lives (GUI.md §5.10).
//!
//! The default is an embedded, in-process server: no socket, no port. Users
//! may opt in to a local daemon (`unix://`, shared with `codex` and the TUI)
//! or a remote app-server over WebSocket (for example across a tailnet).

use codex_app_server_client::RemoteAppServerEndpoint;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Parsed connection choice.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConnectionTarget {
    Embedded,
    Remote(RemoteAppServerEndpoint),
}

impl ConnectionTarget {
    /// Short description for status displays.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Embedded => "Embedded (in-process)".to_string(),
            Self::Remote(RemoteAppServerEndpoint::UnixSocket { socket_path }) => {
                format!("Local daemon ({})", socket_path.display())
            }
            Self::Remote(RemoteAppServerEndpoint::WebSocket { websocket_url, .. }) => {
                format!("Remote ({websocket_url})")
            }
        }
    }

    pub(crate) fn is_embedded(&self) -> bool {
        matches!(self, Self::Embedded)
    }

    /// Whether the server's files live on another machine (a WebSocket
    /// server, as in the TUI's `--remote`): local paths such as pasted
    /// images mean nothing there. Embedded and `unix://` daemons share this
    /// machine's filesystem.
    pub(crate) fn uses_remote_workspace(&self) -> bool {
        matches!(
            self,
            Self::Remote(RemoteAppServerEndpoint::WebSocket { .. })
        )
    }
}

/// Parses a connection address.
///
/// Accepts an empty string or `embedded`, `unix://` (the default daemon
/// socket in `codex_home`), `unix://PATH`, `ws://host:port`, and
/// `wss://host:port`. `auth_token` is attached only to WebSocket endpoints
/// where sending it is safe (TLS, or plain WebSocket to a loopback host).
pub(crate) fn parse_connection(
    address: &str,
    codex_home: Option<&std::path::Path>,
    auth_token: Option<String>,
) -> anyhow::Result<ConnectionTarget> {
    let address = address.trim();
    if address.is_empty() || address.eq_ignore_ascii_case("embedded") {
        return Ok(ConnectionTarget::Embedded);
    }
    if let Some(socket_path) = address.strip_prefix("unix://") {
        let socket_path = if socket_path.is_empty() {
            let codex_home = codex_home
                .ok_or_else(|| anyhow::anyhow!("CODEX_HOME is not available for unix://"))?;
            codex_app_server_client::app_server_control_socket_path(codex_home)?
        } else {
            AbsolutePathBuf::relative_to_current_dir(socket_path)?
        };
        return Ok(ConnectionTarget::Remote(
            RemoteAppServerEndpoint::UnixSocket { socket_path },
        ));
    }
    let parsed = url::Url::parse(address).map_err(|_| invalid_address(address))?;
    let has_host = parsed.host_str().is_some_and(|host| !host.is_empty());
    let plain_root =
        parsed.path() == "/" && parsed.query().is_none() && parsed.fragment().is_none();
    if !matches!(parsed.scheme(), "ws" | "wss")
        || !has_host
        || parsed.port().is_none() && !address_has_explicit_port(address)
        || !plain_root
    {
        return Err(invalid_address(address));
    }
    if auth_token.is_some() && !websocket_url_allows_auth_token(&parsed) {
        anyhow::bail!(
            "refusing to send an auth token over unencrypted {address}; use wss:// or a loopback host"
        );
    }
    Ok(ConnectionTarget::Remote(
        RemoteAppServerEndpoint::WebSocket {
            websocket_url: parsed.to_string(),
            auth_token,
        },
    ))
}

/// Where the connection settings come from: `--remote` /
/// `--remote-auth-token-env` win over the address saved in `gui.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConnectionChoice {
    pub(crate) cli_address: Option<String>,
    pub(crate) cli_token_env: Option<String>,
    pub(crate) saved_address: String,
    pub(crate) saved_token_env: Option<String>,
}

impl ConnectionChoice {
    /// Whether the address comes from `gui.json` rather than the command line.
    pub(crate) fn uses_saved_address(&self) -> bool {
        self.cli_address.is_none()
    }

    /// Resolves the target, reading the token variable through `env` only
    /// for WebSocket endpoints. A token variable saved with a remote address
    /// is ignored when `--remote` names another server.
    pub(crate) fn resolve(
        &self,
        codex_home: Option<&std::path::Path>,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<ConnectionTarget> {
        let (address, token_env) = match &self.cli_address {
            Some(address) => (address.as_str(), self.cli_token_env.as_deref()),
            None => (self.saved_address.as_str(), self.saved_token_env.as_deref()),
        };
        let target = parse_connection(address, codex_home, /*auth_token*/ None)?;
        let is_websocket = matches!(
            target,
            ConnectionTarget::Remote(RemoteAppServerEndpoint::WebSocket { .. })
        );
        match token_env.map(str::trim).filter(|var| !var.is_empty()) {
            Some(var) if is_websocket => {
                let token = env(var).ok_or_else(|| {
                    anyhow::anyhow!(
                        "environment variable {var} (the auth token for {}) is not set in this app's environment",
                        address.trim()
                    )
                })?;
                parse_connection(address, codex_home, Some(token))
            }
            _ => Ok(target),
        }
    }

    /// Status-overlay text for a connection that cannot be resolved.
    pub(crate) fn error_message(&self, err: &anyhow::Error) -> String {
        let source = if self.uses_saved_address() {
            "The app-server connection saved under Settings › Connection"
        } else {
            "The --remote connection"
        };
        format!(
            "{source} cannot be used: {err:#}. Change it under Settings › Connection, or use the embedded server."
        )
    }
}

fn invalid_address(address: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "invalid app-server address `{address}`; expected `embedded`, `unix://`, `unix://PATH`, `ws://host:port`, or `wss://host:port`"
    )
}

/// `Url::port()` hides the scheme's default port; accept it when written out.
fn address_has_explicit_port(address: &str) -> bool {
    let Some((_, rest)) = address.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host_and_port)| host_and_port);
    match host_and_port.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty() && !host.ends_with(':') && port.parse::<u16>().is_ok()
        }
        None => false,
    }
}

fn websocket_url_allows_auth_token(parsed: &url::Url) -> bool {
    match (parsed.scheme(), parsed.host()) {
        ("wss", Some(_)) => true,
        ("ws", Some(url::Host::Domain(domain))) => domain.eq_ignore_ascii_case("localhost"),
        ("ws", Some(url::Host::Ipv4(addr))) => addr.is_loopback(),
        ("ws", Some(url::Host::Ipv6(addr))) => addr.is_loopback(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn empty_and_embedded_mean_in_process() -> anyhow::Result<()> {
        assert_eq!(
            parse_connection("", None, None)?,
            ConnectionTarget::Embedded
        );
        assert_eq!(
            parse_connection(" Embedded ", None, None)?,
            ConnectionTarget::Embedded
        );
        Ok(())
    }

    #[test]
    fn websocket_addresses_need_an_explicit_port() -> anyhow::Result<()> {
        let target = parse_connection("ws://127.0.0.1:4500", None, None)?;
        assert_eq!(
            target,
            ConnectionTarget::Remote(RemoteAppServerEndpoint::WebSocket {
                websocket_url: "ws://127.0.0.1:4500/".to_string(),
                auth_token: None,
            })
        );
        assert!(parse_connection("ws://example.com", None, None).is_err());
        assert!(parse_connection("http://example.com:80", None, None).is_err());
        assert!(parse_connection("wss://example.com:443/path", None, None).is_err());
        assert!(parse_connection("wss://example.com:443", None, None).is_ok());
        Ok(())
    }

    #[test]
    fn auth_tokens_only_over_tls_or_loopback() {
        let token = || Some("secret".to_string());
        assert!(parse_connection("wss://box.tailnet:4500", None, token()).is_ok());
        assert!(parse_connection("ws://localhost:4500", None, token()).is_ok());
        assert!(parse_connection("ws://[::1]:4500", None, token()).is_ok());
        assert!(parse_connection("ws://box.tailnet:4500", None, token()).is_err());
    }

    fn choice(
        cli: Option<&str>,
        cli_token: Option<&str>,
        saved: &str,
        saved_token: Option<&str>,
    ) -> ConnectionChoice {
        ConnectionChoice {
            cli_address: cli.map(str::to_string),
            cli_token_env: cli_token.map(str::to_string),
            saved_address: saved.to_string(),
            saved_token_env: saved_token.map(str::to_string),
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn token_env_is_only_read_for_websocket_targets() -> anyhow::Result<()> {
        // `--remote embedded` must not fail on the saved remote's token.
        let embedded = choice(Some("embedded"), None, "wss://box:4500", Some("TOKEN"));
        assert_eq!(embedded.resolve(None, no_env)?, ConnectionTarget::Embedded);
        assert!(!embedded.uses_saved_address());

        let home = tempfile::tempdir()?;
        let daemon = choice(None, None, "unix://", Some("TOKEN"));
        assert!(matches!(
            daemon.resolve(Some(home.path()), no_env)?,
            ConnectionTarget::Remote(RemoteAppServerEndpoint::UnixSocket { .. })
        ));
        Ok(())
    }

    #[test]
    fn cli_address_ignores_saved_token_env() -> anyhow::Result<()> {
        let cli = choice(
            Some("ws://127.0.0.1:4500"),
            None,
            "wss://box:4500",
            Some("TOKEN"),
        );
        assert_eq!(
            cli.resolve(None, no_env)?,
            ConnectionTarget::Remote(RemoteAppServerEndpoint::WebSocket {
                websocket_url: "ws://127.0.0.1:4500/".to_string(),
                auth_token: None,
            })
        );
        Ok(())
    }

    #[test]
    fn missing_token_variable_is_an_error_naming_it() {
        let saved = choice(None, None, "wss://box:4500", Some("REMOTE_TOKEN"));
        assert!(saved.uses_saved_address());
        let err = saved.resolve(None, no_env).map(|_| ()).unwrap_err();
        assert!(format!("{err:#}").contains("REMOTE_TOKEN"), "{err:#}");

        let target = saved.resolve(None, |var| (var == "REMOTE_TOKEN").then(|| "t".to_string()));
        assert!(matches!(
            target,
            Ok(ConnectionTarget::Remote(
                RemoteAppServerEndpoint::WebSocket {
                    auth_token: Some(_),
                    ..
                }
            ))
        ));
    }

    #[test]
    fn unix_socket_paths() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let target = parse_connection("unix://", Some(home.path()), None)?;
        assert!(matches!(
            target,
            ConnectionTarget::Remote(RemoteAppServerEndpoint::UnixSocket { .. })
        ));
        assert!(parse_connection("unix://", None, None).is_err());
        Ok(())
    }

    #[test]
    fn only_websocket_servers_use_a_remote_workspace() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        assert!(!ConnectionTarget::Embedded.uses_remote_workspace());
        assert!(!parse_connection("unix://", Some(home.path()), None)?.uses_remote_workspace());
        assert!(parse_connection("wss://box.tailnet:4500", None, None)?.uses_remote_workspace());
        Ok(())
    }
}
