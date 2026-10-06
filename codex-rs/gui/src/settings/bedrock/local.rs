//! Local model servers on the Providers page: detecting Ollama and LM Studio,
//! listing installed models, and pulling Ollama models with progress.
//!
//! The probes reuse `codex-ollama` and `codex-lmstudio`, which read the
//! built-in `ollama` / `lmstudio` providers (with any user overrides) from the
//! running server's config. Everything async here runs on Tokio.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_client::legacy_core::config::Config;
use codex_ollama::OllamaClient;
use codex_ollama::PullEvent;
use futures::StreamExt;

use super::flow::LMSTUDIO_PROVIDER_ID;
use super::flow::OLLAMA_PROVIDER_ID;

/// Minimum spacing between progress updates sent to the UI thread.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LocalKind {
    Ollama,
    LmStudio,
}

impl LocalKind {
    pub(crate) const ALL: [LocalKind; 2] = [LocalKind::Ollama, LocalKind::LmStudio];

    pub(crate) fn provider_id(self) -> &'static str {
        match self {
            Self::Ollama => OLLAMA_PROVIDER_ID,
            Self::LmStudio => LMSTUDIO_PROVIDER_ID,
        }
    }

    pub(crate) fn from_provider_id(provider_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.provider_id() == provider_id)
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
        }
    }
}

/// What the page knows about one local server.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum LocalState {
    #[default]
    Unknown,
    Checking,
    Running(LocalProbe),
    Unavailable(String),
}

/// Result of a successful probe.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LocalProbe {
    pub(crate) base_url: String,
    pub(crate) version: Option<String>,
    pub(crate) models: Vec<String>,
    /// Set when the server works but cannot serve Codex (too old).
    pub(crate) warning: Option<String>,
}

/// Checks whether the server for `kind` is reachable and lists its models.
pub(crate) async fn probe(kind: LocalKind, config: Arc<Config>) -> Result<LocalProbe, String> {
    let base_url = config
        .model_providers
        .get(kind.provider_id())
        .and_then(|provider| provider.base_url.clone())
        .unwrap_or_default();
    match kind {
        LocalKind::Ollama => {
            let client = OllamaClient::try_from_oss_provider(&config)
                .await
                .map_err(|err| err.to_string())?;
            let version = client
                .fetch_version()
                .await
                .ok()
                .flatten()
                .map(|version| version.to_string());
            let warning = codex_ollama::ensure_responses_supported(&client)
                .await
                .err()
                .map(|err| err.to_string());
            let mut models = client.fetch_models().await.map_err(|err| err.to_string())?;
            models.sort();
            Ok(LocalProbe {
                base_url,
                version,
                models,
                warning,
            })
        }
        LocalKind::LmStudio => {
            let client = codex_lmstudio::LMStudioClient::try_from_provider(&config)
                .await
                .map_err(|err| err.to_string())?;
            let mut models = client.fetch_models().await.map_err(|err| err.to_string())?;
            models.sort();
            Ok(LocalProbe {
                base_url,
                version: None,
                models,
                warning: None,
            })
        }
    }
}

/// Snapshot of a running pull for the UI.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PullUpdate {
    pub(crate) status: String,
    /// `None` until Ollama reports layer sizes.
    pub(crate) fraction: Option<f32>,
}

/// Pulls `model` with Ollama, reporting throttled progress through
/// `on_progress`. Cancel by aborting the task that awaits this future:
/// dropping the stream closes the connection, which stops the pull.
pub(crate) async fn pull_ollama_model(
    config: Arc<Config>,
    model: String,
    on_progress: impl Fn(PullUpdate),
) -> Result<(), String> {
    let client = OllamaClient::try_from_oss_provider(&config)
        .await
        .map_err(|err| err.to_string())?;
    let mut stream = client
        .pull_model_stream(&model)
        .await
        .map_err(|err| err.to_string())?;
    let mut progress = PullProgress::default();
    let mut last_sent: Option<Instant> = None;
    while let Some(event) = stream.next().await {
        match progress.apply(&event) {
            PullOutcome::Continue => {
                if last_sent.is_none_or(|sent| sent.elapsed() >= PROGRESS_INTERVAL) {
                    last_sent = Some(Instant::now());
                    on_progress(progress.update());
                }
            }
            PullOutcome::Succeeded => return Ok(()),
            PullOutcome::Failed(message) => return Err(message),
        }
    }
    Err("The pull stopped before Ollama reported success.".to_string())
}

/// Whether `name` looks like an Ollama model reference
/// (`[namespace/]model[:tag]`, letters, digits and `._-:/`).
pub(crate) fn is_valid_ollama_model_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with(['/', ':', '.', '-'])
        && !name.ends_with(['/', ':'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'))
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PullOutcome {
    Continue,
    Succeeded,
    Failed(String),
}

/// Aggregates Ollama pull events into one status line and fraction.
#[derive(Debug, Default)]
pub(crate) struct PullProgress {
    status: String,
    /// `(total, completed)` bytes per layer digest.
    layers: HashMap<String, (u64, u64)>,
}

impl PullProgress {
    pub(crate) fn apply(&mut self, event: &PullEvent) -> PullOutcome {
        match event {
            PullEvent::Status(status) => {
                self.status = status.trim().to_string();
                PullOutcome::Continue
            }
            PullEvent::ChunkProgress {
                digest,
                total,
                completed,
            } => {
                let layer = self.layers.entry(digest.clone()).or_insert((0, 0));
                if let Some(total) = total {
                    layer.0 = *total;
                }
                if let Some(completed) = completed {
                    layer.1 = *completed;
                }
                PullOutcome::Continue
            }
            PullEvent::Success => PullOutcome::Succeeded,
            PullEvent::Error(message) => PullOutcome::Failed(message.clone()),
        }
    }

    fn totals(&self) -> (u64, u64) {
        self.layers.values().fold((0, 0), |(total, done), (t, c)| {
            (total + t, done + (*c).min(*t))
        })
    }

    pub(crate) fn fraction(&self) -> Option<f32> {
        let (total, done) = self.totals();
        (total > 0).then(|| (done as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }

    pub(crate) fn update(&self) -> PullUpdate {
        let (total, done) = self.totals();
        let status = if total > 0 {
            let percent = done as f64 * 100.0 / total as f64;
            format!(
                "Downloading {} of {} ({percent:.0}%)",
                format_bytes(done),
                format_bytes(total)
            )
        } else if self.status.is_empty() {
            "Starting…".to_string()
        } else {
            capitalize(&self.status)
        };
        PullUpdate {
            status,
            fraction: self.fraction(),
        }
    }
}

/// `1536` → `1.5 KB`, decimal units like Ollama's CLI.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn chunk(digest: &str, total: Option<u64>, completed: Option<u64>) -> PullEvent {
        PullEvent::ChunkProgress {
            digest: digest.to_string(),
            total,
            completed,
        }
    }

    #[test]
    fn kinds_map_to_built_in_providers() {
        assert_eq!(LocalKind::Ollama.provider_id(), "ollama");
        assert_eq!(
            LocalKind::from_provider_id("lmstudio"),
            Some(LocalKind::LmStudio)
        );
        assert_eq!(LocalKind::from_provider_id("openai"), None);
    }

    #[test]
    fn progress_sums_layers_and_reports_status() {
        let mut progress = PullProgress::default();
        assert_eq!(
            progress.update(),
            PullUpdate {
                status: "Starting…".to_string(),
                fraction: None,
            }
        );
        assert_eq!(
            progress.apply(&PullEvent::Status("pulling manifest".to_string())),
            PullOutcome::Continue
        );
        assert_eq!(progress.update().status, "Pulling manifest");
        progress.apply(&chunk("sha256:a", Some(3_000_000_000), Some(0)));
        progress.apply(&chunk("sha256:b", Some(1_000_000_000), None));
        progress.apply(&chunk("sha256:a", None, Some(1_000_000_000)));
        assert_eq!(
            progress.update(),
            PullUpdate {
                status: "Downloading 1.0 GB of 4.0 GB (25%)".to_string(),
                fraction: Some(0.25),
            }
        );
        // Completed beyond total is clamped per layer.
        progress.apply(&chunk("sha256:b", None, Some(5_000_000_000)));
        assert_eq!(progress.fraction(), Some(0.5));
    }

    #[test]
    fn progress_ends_on_success_or_error() {
        let mut progress = PullProgress::default();
        assert_eq!(progress.apply(&PullEvent::Success), PullOutcome::Succeeded);
        assert_eq!(
            progress.apply(&PullEvent::Error(
                "pull model manifest: file does not exist".to_string()
            )),
            PullOutcome::Failed("pull model manifest: file does not exist".to_string())
        );
    }

    #[test]
    fn formats_bytes_with_decimal_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(2_600_000_000), "2.6 GB");
    }

    #[test]
    fn validates_ollama_model_names() {
        for name in [
            "gpt-oss:20b",
            "llama3.2",
            "library/qwen3:8b-q4_K_M",
            " mistral ",
        ] {
            assert!(is_valid_ollama_model_name(name), "{name} rejected");
        }
        for name in ["", "  ", "gpt oss", ":tag", "model:", "/x", "rm -rf", "a;b"] {
            assert!(!is_valid_ollama_model_name(name), "{name} accepted");
        }
    }
}
