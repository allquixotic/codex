//! Pure logic behind the Providers page: Bedrock endpoints and regions, the
//! credential methods offered after discovery, form validation, and the
//! ordered steps (RPCs and config edits) each provider change performs.
//!
//! Nothing here touches Slint or the backend so it can be unit tested.

use std::collections::VecDeque;

use codex_app_server_protocol::Account;
use codex_app_server_protocol::AwsCredentialType;
use codex_app_server_protocol::BedrockDiscoverResponse;
use codex_app_server_protocol::BedrockSetupParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigLayer;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::LoginAccountParams;
use codex_app_server_protocol::MergeStrategy;
use codex_model_provider::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider::AMAZON_BEDROCK_RUNTIME_PROVIDER_ID;
use serde_json::Value as JsonValue;

/// Provider id Codex uses when `model_provider` is unset.
pub(crate) const OPENAI_PROVIDER_ID: &str = "openai";
pub(crate) const OLLAMA_PROVIDER_ID: &str = "ollama";
pub(crate) const LMSTUDIO_PROVIDER_ID: &str = "lmstudio";

/// Guidance shown with the GovCloud warning (same link as the TUI).
pub(crate) const GOV_CLOUD_GUIDANCE_URL: &str =
    "https://learn.chatgpt.com/docs/enterprise/govcloud-configuration";
/// Setup guide for AWS credentials (same link as the TUI).
pub(crate) const BEDROCK_SETUP_GUIDE_URL: &str = "https://learn.chatgpt.com/docs/amazon-bedrock";

/// Region preselected when nothing better is known.
pub(crate) const DEFAULT_REGION: &str = "us-east-1";

/// Regions where Amazon Bedrock Mantle is available, in the order of
/// `BEDROCK_MANTLE_SUPPORTED_REGIONS` (`model-provider/src/amazon_bedrock/mantle.rs`).
/// The setup and login RPCs validate against this list for both endpoints.
const MANTLE_REGIONS: [(&str, &str); 12] = [
    ("us-east-2", "US East (Ohio)"),
    ("us-east-1", "US East (N. Virginia)"),
    ("us-west-2", "US West (Oregon)"),
    ("ap-southeast-3", "Asia Pacific (Jakarta)"),
    ("ap-south-1", "Asia Pacific (Mumbai)"),
    ("ap-northeast-1", "Asia Pacific (Tokyo)"),
    ("eu-central-1", "Europe (Frankfurt)"),
    ("eu-west-1", "Europe (Ireland)"),
    ("eu-west-2", "Europe (London)"),
    ("eu-south-1", "Europe (Milan)"),
    ("eu-north-1", "Europe (Stockholm)"),
    ("sa-east-1", "South America (São Paulo)"),
];

/// AWS GovCloud regions Bedrock supports (`BEDROCK_GOV_CLOUD_SUPPORTED_REGIONS`).
const GOV_CLOUD_REGIONS: [(&str, &str); 2] = [
    ("us-gov-east-1", "AWS GovCloud (US-East)"),
    ("us-gov-west-1", "AWS GovCloud (US-West)"),
];

/// One entry of the region picker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionOption {
    pub(crate) code: &'static str,
    pub(crate) name: &'static str,
    pub(crate) gov_cloud: bool,
}

impl RegionOption {
    pub(crate) fn label(&self) -> String {
        format!("{} · {}", self.code, self.name)
    }
}

/// Mantle regions followed by the GovCloud regions.
pub(crate) fn region_options() -> Vec<RegionOption> {
    MANTLE_REGIONS
        .iter()
        .map(|(code, name)| RegionOption {
            code,
            name,
            gov_cloud: false,
        })
        .chain(GOV_CLOUD_REGIONS.iter().map(|(code, name)| RegionOption {
            code,
            name,
            gov_cloud: true,
        }))
        .collect()
}

/// Index of `region` (trimmed, case-insensitive) in [`region_options`].
pub(crate) fn region_index(region: &str) -> Option<usize> {
    let region = region.trim();
    region_options()
        .iter()
        .position(|option| option.code.eq_ignore_ascii_case(region))
}

pub(crate) fn is_supported_region(region: &str) -> bool {
    codex_model_provider::is_supported_amazon_bedrock_region(region.trim())
}

pub(crate) fn is_gov_cloud_region(region: &str) -> bool {
    codex_model_provider::is_amazon_bedrock_gov_cloud_region(region.trim())
}

/// Bedrock endpoint flavor, one built-in provider each.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Endpoint {
    /// `amazon-bedrock`: `bedrock-mantle.{region}.api.aws`.
    #[default]
    Mantle,
    /// `amazon-bedrock-runtime`: `bedrock-runtime.{region}.amazonaws.com`.
    Runtime,
}

impl Endpoint {
    pub(crate) fn provider_id(self) -> &'static str {
        match self {
            Self::Mantle => AMAZON_BEDROCK_PROVIDER_ID,
            Self::Runtime => AMAZON_BEDROCK_RUNTIME_PROVIDER_ID,
        }
    }

    pub(crate) fn from_provider_id(provider_id: &str) -> Option<Self> {
        match provider_id {
            AMAZON_BEDROCK_PROVIDER_ID => Some(Self::Mantle),
            AMAZON_BEDROCK_RUNTIME_PROVIDER_ID => Some(Self::Runtime),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Mantle => "Mantle",
            Self::Runtime => "Runtime",
        }
    }

    /// One-line description for the endpoint picker.
    pub(crate) fn description(self, region: &str) -> String {
        match self {
            Self::Mantle => format!(
                "Provider amazon-bedrock. Requests go to {}, the OpenAI-compatible Bedrock endpoint. Recommended.",
                self.host(region)
            ),
            Self::Runtime => format!(
                "Provider amazon-bedrock-runtime. Requests go to {} using global. and us. cross-region inference profiles.",
                self.host(region)
            ),
        }
    }

    pub(crate) fn host(self, region: &str) -> String {
        match self {
            Self::Mantle => format!("bedrock-mantle.{region}.api.aws"),
            Self::Runtime => format!("bedrock-runtime.{region}.amazonaws.com"),
        }
    }
}

pub(crate) fn is_bedrock_provider(provider_id: &str) -> bool {
    Endpoint::from_provider_id(provider_id).is_some()
}

/// A way to authenticate with AWS, as offered in the credential list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Method {
    /// The discovered profile at this index of `BedrockDiscoverResponse::profiles`.
    Profile(usize),
    /// `AWS_*` variables found in the environment.
    Environment,
    /// A profile name typed by the user (SSO session or one not in the files).
    ManualProfile,
    /// Access keys stored by Codex (`amazonBedrockAccessKeys` login).
    AccessKeys,
    /// A Bedrock API key stored by Codex (`amazonBedrock` login).
    ApiKey,
}

/// Credential methods in display order, following the TUI wizard: detected
/// profiles first (discovery puts the selected profile first), detected
/// environment credentials next, then the methods that need input.
pub(crate) fn method_list(discovered: &BedrockDiscoverResponse) -> Vec<Method> {
    let mut methods: Vec<Method> = (0..discovered.profiles.len())
        .map(Method::Profile)
        .collect();
    if !discovered.environment_credentials.is_empty() {
        methods.push(Method::Environment);
    }
    methods.extend([Method::ManualProfile, Method::AccessKeys, Method::ApiKey]);
    methods
}

/// Which input fields a method shows when selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MethodFields {
    None,
    ProfileName,
    AccessKeys,
    ApiKey,
}

impl MethodFields {
    /// Code shared with `BedrockMethodItem.fields` in `ui/bedrock.slint`.
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::ProfileName => 1,
            Self::AccessKeys => 2,
            Self::ApiKey => 3,
        }
    }
}

/// Display data for one credential method.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MethodView {
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) badges: Vec<String>,
    pub(crate) fields: MethodFields,
}

/// Describes `method`. `selected_profile` is the profile the AWS SDK would
/// pick (`AWS_PROFILE`, else `default`).
pub(crate) fn describe_method(
    method: Method,
    discovered: &BedrockDiscoverResponse,
    selected_profile: &str,
) -> MethodView {
    match method {
        Method::Profile(index) => {
            let profile = discovered.profiles.get(index);
            let name = profile
                .map(|profile| profile.name.clone())
                .unwrap_or_default();
            let detail = match profile.and_then(|profile| non_empty(profile.region.as_deref())) {
                Some(region) => format!("AWS profile · {region}"),
                None => "AWS profile · no region configured".to_string(),
            };
            let badges = if name == selected_profile {
                vec!["Active".to_string()]
            } else {
                Vec::new()
            };
            MethodView {
                title: name,
                detail,
                badges,
                fields: MethodFields::None,
            }
        }
        Method::Environment => {
            let region = discovered
                .environment_credentials
                .iter()
                .find_map(|credential| non_empty(credential.region.as_deref()));
            let detail = match region {
                Some(region) => format!("From AWS environment variables · {region}"),
                None => "From AWS environment variables".to_string(),
            };
            let badges = discovered
                .environment_credentials
                .iter()
                .map(|credential| match credential.credential_type {
                    AwsCredentialType::AccessKeys => "Access keys".to_string(),
                    AwsCredentialType::BedrockApiKey => "Bedrock API key".to_string(),
                })
                .collect();
            MethodView {
                title: "Environment credentials".to_string(),
                detail,
                badges,
                fields: MethodFields::None,
            }
        }
        Method::ManualProfile => MethodView {
            title: if discovered.profiles.is_empty() {
                "AWS profile".to_string()
            } else {
                "Other AWS profile".to_string()
            },
            detail: "A named profile or AWS SSO session from your AWS config".to_string(),
            badges: Vec::new(),
            fields: MethodFields::ProfileName,
        },
        Method::AccessKeys => MethodView {
            title: "AWS access keys".to_string(),
            detail: "An access key ID and secret access key, stored by Codex".to_string(),
            badges: Vec::new(),
            fields: MethodFields::AccessKeys,
        },
        Method::ApiKey => MethodView {
            title: "Bedrock API key".to_string(),
            detail: "An Amazon Bedrock API key, stored by Codex".to_string(),
            badges: Vec::new(),
            fields: MethodFields::ApiKey,
        },
    }
}

/// Profile the AWS SDK selects: `AWS_PROFILE` when set, else `default`.
pub(crate) fn selected_profile_name(aws_profile_env: Option<&str>) -> String {
    non_empty(aws_profile_env).unwrap_or("default").to_string()
}

/// Region to preselect for `method` from discovery, as the TUI does: the
/// profile's own region, or the first region among environment credentials.
pub(crate) fn suggested_region(
    method: Method,
    discovered: &BedrockDiscoverResponse,
) -> Option<String> {
    match method {
        Method::Profile(index) => discovered
            .profiles
            .get(index)
            .and_then(|profile| non_empty(profile.region.as_deref()))
            .map(str::to_string),
        Method::Environment => discovered
            .environment_credentials
            .iter()
            .find_map(|credential| non_empty(credential.region.as_deref()))
            .map(str::to_string),
        Method::ManualProfile | Method::AccessKeys | Method::ApiKey => None,
    }
}

/// Values typed into the Bedrock form.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FormInput {
    pub(crate) endpoint: Endpoint,
    pub(crate) method: Option<Method>,
    pub(crate) manual_profile: String,
    pub(crate) api_key: String,
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: String,
    pub(crate) region: String,
}

/// What validation needs besides the typed values.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormContext<'a> {
    pub(crate) discovered: &'a BedrockDiscoverResponse,
    /// Codex already stores Bedrock credentials (they outrank environment
    /// credentials, so the server rejects environment setup).
    pub(crate) managed_credentials: bool,
    /// Discovery succeeded, so `discovered.profiles` lists every profile in
    /// the AWS config and credentials files.
    pub(crate) discovery_complete: bool,
}

/// The credential a validated form resolved to.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum Credential {
    Profile(String),
    Environment,
    ApiKey(String),
    AccessKeys {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Profile(profile) => f.debug_tuple("Profile").field(profile).finish(),
            Self::Environment => f.write_str("Environment"),
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::AccessKeys { .. } => f.write_str("AccessKeys(<redacted>)"),
        }
    }
}

/// A complete, validated Bedrock configuration request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetupPlan {
    pub(crate) endpoint: Endpoint,
    pub(crate) credential: Credential,
    pub(crate) region: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FormError {
    NoMethod,
    MissingProfile,
    InvalidProfile,
    UnknownProfile(String),
    NoEnvironmentCredentials,
    ManagedCredentialsTakePriority,
    MissingAccessKeyId,
    InvalidAccessKeyId,
    MissingSecretAccessKey,
    InvalidSecret,
    MissingApiKey,
    MissingRegion,
    UnsupportedRegion(String),
}

impl std::fmt::Display for FormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMethod => f.write_str("Choose how Codex should authenticate with AWS."),
            Self::MissingProfile => f.write_str("Enter the name of an AWS profile."),
            Self::InvalidProfile => f.write_str("AWS profile names cannot contain spaces."),
            Self::UnknownProfile(profile) => write!(
                f,
                "AWS profile \"{profile}\" is not in your AWS config or credentials files. Create it with aws configure sso or aws configure --profile {profile}, then scan again."
            ),
            Self::NoEnvironmentCredentials => f.write_str(
                "No AWS credentials were found in the environment. Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY or AWS_BEARER_TOKEN_BEDROCK, then restart Codex.",
            ),
            Self::ManagedCredentialsTakePriority => f.write_str(
                "Codex already stores Amazon Bedrock credentials, and they take priority over environment credentials. Switch back to OpenAI to remove them first, or choose another method.",
            ),
            Self::MissingAccessKeyId => f.write_str("Enter an AWS access key ID."),
            Self::InvalidAccessKeyId => f.write_str(
                "AWS access key IDs are 16 to 128 letters and digits, for example AKIA… or ASIA….",
            ),
            Self::MissingSecretAccessKey => f.write_str("Enter the AWS secret access key."),
            Self::InvalidSecret => f.write_str("Keys and tokens cannot contain spaces or line breaks."),
            Self::MissingApiKey => f.write_str("Enter an Amazon Bedrock API key."),
            Self::MissingRegion => f.write_str("Choose an AWS Region."),
            Self::UnsupportedRegion(region) => {
                write!(f, "Amazon Bedrock does not support region {region}.")
            }
        }
    }
}

/// Checks the form and resolves it to a [`SetupPlan`].
pub(crate) fn validate_form(
    input: &FormInput,
    context: FormContext<'_>,
) -> Result<SetupPlan, FormError> {
    let credential = match input.method.ok_or(FormError::NoMethod)? {
        Method::Profile(index) => {
            let profile = context
                .discovered
                .profiles
                .get(index)
                .ok_or(FormError::NoMethod)?;
            Credential::Profile(profile.name.clone())
        }
        Method::ManualProfile => {
            let profile = input.manual_profile.trim();
            if profile.is_empty() {
                return Err(FormError::MissingProfile);
            }
            if profile.chars().any(char::is_whitespace) {
                return Err(FormError::InvalidProfile);
            }
            if context.discovery_complete
                && !context
                    .discovered
                    .profiles
                    .iter()
                    .any(|known| known.name == profile)
            {
                return Err(FormError::UnknownProfile(profile.to_string()));
            }
            Credential::Profile(profile.to_string())
        }
        Method::Environment => {
            if context.discovered.environment_credentials.is_empty() {
                return Err(FormError::NoEnvironmentCredentials);
            }
            if context.managed_credentials {
                return Err(FormError::ManagedCredentialsTakePriority);
            }
            Credential::Environment
        }
        Method::AccessKeys => {
            let access_key_id = input.access_key_id.trim();
            let secret_access_key = input.secret_access_key.trim();
            let session_token = input.session_token.trim();
            if access_key_id.is_empty() {
                return Err(FormError::MissingAccessKeyId);
            }
            if !(16..=128).contains(&access_key_id.len())
                || !access_key_id.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Err(FormError::InvalidAccessKeyId);
            }
            if secret_access_key.is_empty() {
                return Err(FormError::MissingSecretAccessKey);
            }
            if has_whitespace(secret_access_key) || has_whitespace(session_token) {
                return Err(FormError::InvalidSecret);
            }
            Credential::AccessKeys {
                access_key_id: access_key_id.to_string(),
                secret_access_key: secret_access_key.to_string(),
                session_token: (!session_token.is_empty()).then(|| session_token.to_string()),
            }
        }
        Method::ApiKey => {
            let api_key = input.api_key.trim();
            if api_key.is_empty() {
                return Err(FormError::MissingApiKey);
            }
            if has_whitespace(api_key) {
                return Err(FormError::InvalidSecret);
            }
            Credential::ApiKey(api_key.to_string())
        }
    };
    let region = input.region.trim();
    if region.is_empty() {
        return Err(FormError::MissingRegion);
    }
    if !is_supported_region(region) {
        return Err(FormError::UnsupportedRegion(region.to_string()));
    }
    Ok(SetupPlan {
        endpoint: input.endpoint,
        credential,
        region: region.to_string(),
    })
}

/// One unit of work in a provider change. Steps run in order; the first
/// failure stops the change.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    /// Resolve the profile's credentials locally (`validate_aws_profile`).
    /// Used where no server RPC validates the profile (Runtime).
    ValidateProfile { profile: String, region: String },
    /// `account/bedrock/setup` (Mantle with a profile or environment).
    Setup(BedrockSetupParams),
    /// `account/login/start` with a Bedrock variant (stores credentials).
    Login(LoginAccountParams),
    /// `config/batchWrite` to the user config. A layer that overrides one
    /// of the edits fails the change.
    Write(Vec<ConfigEdit>),
    /// `config/batchWrite` clearing `model`, so new threads start on the
    /// provider's default. The provider change does not depend on it, so a
    /// layer that pins `model` (or a failed write) only adds a warning.
    ClearModel,
    /// `account/logout` (removes Codex-managed credentials).
    Logout,
    /// `account/bedrock/checkGovCloudRequirements`, advisory.
    CheckGovCloud,
    /// Restart the embedded server so the new provider takes effect.
    Restart,
    /// `model/list` from the restarted server.
    LoadModels,
}

impl Step {
    /// Progress text shown while the step runs.
    pub(crate) fn progress_label(&self) -> &'static str {
        match self {
            Self::ValidateProfile { .. } => "Checking the AWS profile's credentials…",
            Self::Setup(_) => "Setting up Amazon Bedrock…",
            Self::Login(_) => "Saving Amazon Bedrock credentials…",
            Self::Write(_) | Self::ClearModel => "Updating config.toml…",
            Self::Logout => "Removing stored Amazon Bedrock credentials…",
            Self::CheckGovCloud => "Checking AWS GovCloud requirements…",
            Self::Restart => "Restarting Codex…",
            Self::LoadModels => "Loading models…",
        }
    }

    /// Whether the user may cancel while this step runs. Steps that write
    /// configuration or credentials cannot be undone halfway.
    pub(crate) fn is_cancellable(&self) -> bool {
        matches!(self, Self::ValidateProfile { .. })
    }
}

/// Steps to configure Bedrock as described by `plan`. `current_provider` is
/// the provider configured before the change.
pub(crate) fn plan_apply_steps(plan: &SetupPlan, current_provider: &str) -> Vec<Step> {
    let region = plan.region.clone();
    let mut steps = Vec::new();
    match (&plan.credential, plan.endpoint) {
        (Credential::Profile(profile), Endpoint::Mantle) => {
            steps.push(Step::Setup(BedrockSetupParams::Profile {
                profile: profile.clone(),
                region,
            }));
        }
        (Credential::Environment, Endpoint::Mantle) => {
            steps.push(Step::Setup(BedrockSetupParams::Environment { region }));
        }
        (Credential::Profile(profile), Endpoint::Runtime) => {
            steps.push(Step::ValidateProfile {
                profile: profile.clone(),
                region,
            });
        }
        (Credential::Environment, Endpoint::Runtime) => {}
        (Credential::ApiKey(api_key), _) => {
            steps.push(Step::Login(LoginAccountParams::AmazonBedrock {
                api_key: api_key.clone(),
                region,
            }));
        }
        (
            Credential::AccessKeys {
                access_key_id,
                secret_access_key,
                session_token,
            },
            _,
        ) => {
            steps.push(Step::Login(LoginAccountParams::AmazonBedrockAccessKeys {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
                region,
            }));
        }
    }
    let edits = follow_up_edits(plan);
    if !edits.is_empty() {
        steps.push(Step::Write(edits));
    }
    // A model chosen for another provider would not exist in the new one.
    if current_provider != plan.endpoint.provider_id() {
        steps.push(Step::ClearModel);
    }
    steps.extend([Step::CheckGovCloud, Step::Restart, Step::LoadModels]);
    steps
}

/// Config edits written after the setup or login RPC.
///
/// `account/bedrock/setup` and the Bedrock logins always select the Mantle
/// provider, so Runtime is selected here.
pub(crate) fn follow_up_edits(plan: &SetupPlan) -> Vec<ConfigEdit> {
    let mut edits = Vec::new();
    if plan.endpoint == Endpoint::Runtime {
        let provider = AMAZON_BEDROCK_RUNTIME_PROVIDER_ID;
        let profile = match &plan.credential {
            Credential::Profile(profile) => JsonValue::String(profile.clone()),
            // A configured profile outranks Codex-managed and environment
            // credentials, so it must not linger.
            Credential::Environment | Credential::ApiKey(_) | Credential::AccessKeys { .. } => {
                JsonValue::Null
            }
        };
        edits.push(replace(
            "model_provider",
            JsonValue::String(provider.to_string()),
        ));
        edits.push(replace(
            &format!("model_providers.{provider}.aws.profile"),
            profile,
        ));
        edits.push(replace(
            &format!("model_providers.{provider}.aws.region"),
            JsonValue::String(plan.region.clone()),
        ));
    }
    edits
}

/// Steps to return to the default OpenAI provider. Mirrors what
/// `account/logout` clears for a Bedrock provider.
///
/// Clearing the user's `model_provider` exposes whatever a lower layer
/// (system config, MDM, packaged defaults) selects, so when that is not
/// OpenAI (`fallback_provider`), OpenAI is written explicitly. Stored
/// credentials are removed first: the dialog promised it, and a later
/// failure leaves the provider set so the user can try again.
pub(crate) fn plan_revert_steps(
    current_provider: &str,
    remove_credentials: bool,
    fallback_provider: Option<&str>,
) -> Vec<Step> {
    let provider_edit = match fallback_provider {
        Some(fallback) if fallback != OPENAI_PROVIDER_ID => replace(
            "model_provider",
            JsonValue::String(OPENAI_PROVIDER_ID.to_string()),
        ),
        _ => clear("model_provider"),
    };
    let mut edits = vec![provider_edit];
    if is_bedrock_provider(current_provider) {
        edits.push(clear(&format!("model_providers.{current_provider}.aws")));
    }
    let mut steps = Vec::new();
    if remove_credentials {
        steps.push(Step::Logout);
    }
    steps.extend([Step::Write(edits), Step::ClearModel, Step::Restart]);
    steps
}

/// The edit behind [`Step::ClearModel`].
pub(crate) fn clear_model_edits() -> Vec<ConfigEdit> {
    vec![clear("model")]
}

/// Steps to use `model` from a local server (`ollama` or `lmstudio`).
pub(crate) fn plan_local_steps(provider_id: &str, model: &str) -> Vec<Step> {
    vec![
        Step::Write(vec![
            replace("model_provider", JsonValue::String(provider_id.to_string())),
            replace("model", JsonValue::String(model.to_string())),
        ]),
        Step::Restart,
    ]
}

/// Drops the steps that only make sense for the embedded server: local
/// profile validation (the profile lives on the server's host), restarting,
/// and reloading the catalog. Used when the GUI is connected to a daemon or a
/// remote app-server, which keeps its provider until it restarts.
pub(crate) fn without_embedded_steps(steps: Vec<Step>) -> Vec<Step> {
    steps
        .into_iter()
        .filter(|step| {
            !matches!(
                step,
                Step::ValidateProfile { .. } | Step::Restart | Step::LoadModels
            )
        })
        .collect()
}

/// Edit that persists the default model for new threads.
pub(crate) fn model_edits(model: &str) -> Vec<ConfigEdit> {
    vec![replace("model", JsonValue::String(model.to_string()))]
}

fn replace(key_path: &str, value: JsonValue) -> ConfigEdit {
    ConfigEdit {
        key_path: key_path.to_string(),
        value,
        merge_strategy: MergeStrategy::Replace,
    }
}

/// `null` with `Replace` removes the key (missing keys are fine).
fn clear(key_path: &str) -> ConfigEdit {
    replace(key_path, JsonValue::Null)
}

/// What kind of provider change a [`Flow`] performs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FlowKind {
    Bedrock {
        endpoint: Endpoint,
        region: String,
    },
    Revert,
    Local {
        provider_id: String,
        model: String,
    },
    /// Restart only, to pick up a provider already saved in config.toml.
    Restart,
}

impl FlowKind {
    /// Message shown when the change finished.
    pub(crate) fn success_message(&self) -> String {
        match self {
            Self::Bedrock { endpoint, region } => format!(
                "Amazon Bedrock ({}) is set up in {region}. New threads use it.",
                endpoint.label()
            ),
            Self::Revert => {
                "Codex uses OpenAI again. Sign in on the Account page if needed.".to_string()
            }
            Self::Local { provider_id, model } => format!(
                "New threads use {model} on {}.",
                provider_label(provider_id, /*custom_name*/ None)
            ),
            Self::Restart => "Codex restarted with the provider from config.toml.".to_string(),
        }
    }

    /// Prefix for failures, as in "Unable to set up Amazon Bedrock: …".
    pub(crate) fn failure_prefix(&self) -> &'static str {
        match self {
            Self::Bedrock { .. } => "Unable to set up Amazon Bedrock",
            Self::Revert => "Unable to switch back to OpenAI",
            Self::Local { .. } => "Unable to switch to the local model",
            Self::Restart => "Unable to restart Codex",
        }
    }
}

/// A provider change in progress: an id that tags async results, the
/// steps still to run, and warnings for the final message.
#[derive(Debug)]
pub(crate) struct Flow {
    pub(crate) id: u64,
    pub(crate) kind: FlowKind,
    pending: VecDeque<Step>,
    current: Option<Step>,
    warnings: Vec<String>,
}

impl Flow {
    pub(crate) fn new(id: u64, kind: FlowKind, steps: Vec<Step>) -> Self {
        Self {
            id,
            kind,
            pending: steps.into(),
            current: None,
            warnings: Vec::new(),
        }
    }

    /// Notes something that did not work out but does not stop the change.
    pub(crate) fn warn(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    /// The success message followed by any warnings.
    pub(crate) fn finished_message(&self, success: String) -> String {
        std::iter::once(success)
            .chain(self.warnings.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Moves to the next step and returns it; `None` when the change is done.
    pub(crate) fn advance(&mut self) -> Option<Step> {
        self.current = self.pending.pop_front();
        self.current.clone()
    }

    pub(crate) fn current(&self) -> Option<&Step> {
        self.current.as_ref()
    }

    /// Drops everything after the restart (used when the user postpones it).
    pub(crate) fn skip_restart(&mut self) {
        self.pending.clear();
        self.current = None;
    }
}

/// Provider settings read from `config/read`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderStatus {
    /// Effective provider id (`openai` when unset).
    pub(crate) provider_id: String,
    /// `model_providers.<id>.name`, for custom providers.
    pub(crate) provider_name: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) aws_profile: Option<String>,
    pub(crate) aws_region: Option<String>,
    /// Layer that set `model_provider`, when it is not the base user config.
    pub(crate) provider_origin: Option<String>,
    /// That layer outranks the user config, so writes here do not take effect.
    pub(crate) provider_locked: bool,
    /// Provider a lower layer selects, which applies when the user config
    /// does not set one (needs `config/read` with layers).
    pub(crate) fallback_provider: Option<String>,
}

/// `model_provider` of the highest enabled layer below the user config.
fn fallback_provider(layers: &[ConfigLayer]) -> Option<String> {
    layers
        .iter()
        .filter(|layer| {
            layer.disabled_reason.is_none() && layer.name.precedence() < USER_LAYER_PRECEDENCE
        })
        .filter_map(|layer| {
            let provider = non_empty(layer.config.get("model_provider")?.as_str())?;
            Some((layer.name.precedence(), provider.to_string()))
        })
        .max_by_key(|(precedence, _)| *precedence)
        .map(|(_, provider)| provider)
}

pub(crate) fn provider_status(response: &ConfigReadResponse) -> ProviderStatus {
    let config = serde_json::to_value(&response.config).unwrap_or(JsonValue::Null);
    let provider_id = non_empty(config.get("model_provider").and_then(JsonValue::as_str))
        .unwrap_or(OPENAI_PROVIDER_ID)
        .to_string();
    let provider = config
        .get("model_providers")
        .and_then(|providers| providers.get(&provider_id));
    let string_at = |pointer: &str| {
        provider
            .and_then(|provider| provider.pointer(pointer))
            .and_then(JsonValue::as_str)
            .and_then(|value| non_empty(Some(value)))
            .map(str::to_string)
    };
    let (provider_origin, provider_locked) = match response.origins.get("model_provider") {
        Some(origin) if !matches!(origin.name, ConfigLayerSource::User { profile: None, .. }) => (
            Some(describe_layer(&origin.name)),
            origin.name.precedence() > USER_LAYER_PRECEDENCE,
        ),
        _ => (None, false),
    };
    ProviderStatus {
        provider_name: string_at("/name"),
        model: non_empty(config.get("model").and_then(JsonValue::as_str)).map(str::to_string),
        aws_profile: string_at("/aws/profile"),
        aws_region: string_at("/aws/region"),
        provider_id,
        provider_origin,
        provider_locked,
        fallback_provider: response.layers.as_deref().and_then(fallback_provider),
    }
}

/// Precedence of the base user `config.toml` (`ConfigLayerSource::User`).
const USER_LAYER_PRECEDENCE: i16 = 20;

/// Human description of a config layer, for "set by …" notes.
pub(crate) fn describe_layer(source: &ConfigLayerSource) -> String {
    match source {
        ConfigLayerSource::PackagedDefaults { .. } => "Codex defaults".to_string(),
        ConfigLayerSource::Mdm { domain, .. } => format!("device management ({domain})"),
        ConfigLayerSource::System { file } => format!("system config {}", file.display()),
        ConfigLayerSource::EnterpriseManaged { name, .. } => {
            format!("your organization's managed config ({name})")
        }
        ConfigLayerSource::User {
            profile: Some(profile),
            ..
        } => format!("config profile \"{profile}\""),
        ConfigLayerSource::User {
            file,
            profile: None,
        } => {
            format!("user config {}", file.display())
        }
        ConfigLayerSource::Project { dot_codex_folder } => {
            format!("project config in {}", dot_codex_folder.display())
        }
        ConfigLayerSource::SessionFlags => "command-line flags".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            format!("managed config {}", file.display())
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            "managed config from device management".to_string()
        }
    }
}

/// User-facing provider name.
pub(crate) fn provider_label(provider_id: &str, custom_name: Option<&str>) -> String {
    match provider_id {
        OPENAI_PROVIDER_ID => "OpenAI".to_string(),
        AMAZON_BEDROCK_PROVIDER_ID => "Amazon Bedrock (Mantle)".to_string(),
        AMAZON_BEDROCK_RUNTIME_PROVIDER_ID => "Amazon Bedrock (Runtime)".to_string(),
        OLLAMA_PROVIDER_ID => "Ollama".to_string(),
        LMSTUDIO_PROVIDER_ID => "LM Studio".to_string(),
        other => match non_empty(custom_name) {
            Some(name) if name != other => format!("{name} ({other})"),
            _ => other.to_string(),
        },
    }
}

/// One-line description of `account/read`. `aws_profile` is the profile
/// configured for a Bedrock provider, if any.
pub(crate) fn account_label(response: &GetAccountResponse, aws_profile: Option<&str>) -> String {
    match &response.account {
        Some(Account::ApiKey {}) => "OpenAI API key".to_string(),
        Some(Account::Chatgpt { email, .. }) => match non_empty(email.as_deref()) {
            Some(email) => format!("ChatGPT · {email}"),
            None => "ChatGPT".to_string(),
        },
        Some(Account::AmazonBedrock {
            uses_codex_managed_credentials: true,
        }) => "Amazon Bedrock credentials stored by Codex".to_string(),
        Some(Account::AmazonBedrock {
            uses_codex_managed_credentials: false,
        }) => match non_empty(aws_profile) {
            Some(profile) => format!("Credentials from AWS profile \"{profile}\""),
            None => "AWS credentials from the environment or the AWS SDK chain".to_string(),
        },
        None if response.requires_openai_auth => "Not signed in".to_string(),
        None => "No sign-in required".to_string(),
    }
}

/// Whether Codex stores Bedrock credentials (API key or access keys).
pub(crate) fn uses_managed_credentials(response: Option<&GetAccountResponse>) -> bool {
    matches!(
        response.and_then(|response| response.account.as_ref()),
        Some(Account::AmazonBedrock {
            uses_codex_managed_credentials: true
        })
    )
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn has_whitespace(value: &str) -> bool {
    value.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::BedrockAwsProfile;
    use codex_app_server_protocol::BedrockEnvironmentCredential;
    use codex_app_server_protocol::ConfigLayerMetadata;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;

    fn profile(name: &str, region: Option<&str>) -> BedrockAwsProfile {
        BedrockAwsProfile {
            name: name.to_string(),
            region: region.map(str::to_string),
        }
    }

    fn env(
        credential_type: AwsCredentialType,
        region: Option<&str>,
    ) -> BedrockEnvironmentCredential {
        BedrockEnvironmentCredential {
            credential_type,
            region: region.map(str::to_string),
        }
    }

    fn discovered(
        profiles: Vec<BedrockAwsProfile>,
        environment_credentials: Vec<BedrockEnvironmentCredential>,
    ) -> BedrockDiscoverResponse {
        BedrockDiscoverResponse {
            profiles,
            environment_credentials,
        }
    }

    fn empty() -> BedrockDiscoverResponse {
        discovered(Vec::new(), Vec::new())
    }

    fn context(discovered: &BedrockDiscoverResponse) -> FormContext<'_> {
        FormContext {
            discovered,
            managed_credentials: false,
            discovery_complete: false,
        }
    }

    fn form(method: Method) -> FormInput {
        FormInput {
            method: Some(method),
            region: "us-west-2".to_string(),
            ..FormInput::default()
        }
    }

    #[test]
    fn region_list_matches_the_server_validation() {
        let options = region_options();
        assert_eq!(options.len(), 14);
        for option in &options {
            assert!(is_supported_region(option.code), "{} rejected", option.code);
            assert_eq!(is_gov_cloud_region(option.code), option.gov_cloud);
        }
        assert_eq!(options[1].code, "us-east-1");
        assert_eq!(options[1].label(), "us-east-1 · US East (N. Virginia)");
        assert_eq!(options.iter().filter(|option| option.gov_cloud).count(), 2);
    }

    #[test]
    fn region_lookup_trims_and_ignores_case() {
        assert_eq!(region_index(" US-WEST-2 "), Some(2));
        assert_eq!(region_index("us-gov-west-1"), Some(13));
        assert_eq!(region_index("ap-southeast-2"), None);
        assert!(!is_supported_region("ap-southeast-2"));
        assert!(!is_supported_region(""));
        assert_eq!(region_index(DEFAULT_REGION), Some(1));
    }

    #[test]
    fn endpoints_map_to_built_in_providers() {
        assert_eq!(Endpoint::Mantle.provider_id(), "amazon-bedrock");
        assert_eq!(Endpoint::Runtime.provider_id(), "amazon-bedrock-runtime");
        assert_eq!(
            Endpoint::from_provider_id("amazon-bedrock-runtime"),
            Some(Endpoint::Runtime)
        );
        assert_eq!(Endpoint::from_provider_id("openai"), None);
        assert_eq!(
            Endpoint::Runtime.host("us-east-1"),
            "bedrock-runtime.us-east-1.amazonaws.com"
        );
        assert!(
            Endpoint::Mantle
                .description("eu-west-1")
                .contains("bedrock-mantle.eu-west-1.api.aws")
        );
    }

    #[test]
    fn method_list_puts_detected_credentials_first() {
        let response = discovered(
            vec![profile("default", Some("us-east-1")), profile("dev", None)],
            vec![env(AwsCredentialType::AccessKeys, None)],
        );
        assert_eq!(
            method_list(&response),
            vec![
                Method::Profile(0),
                Method::Profile(1),
                Method::Environment,
                Method::ManualProfile,
                Method::AccessKeys,
                Method::ApiKey,
            ]
        );
    }

    #[test]
    fn method_list_without_profiles_starts_with_environment() {
        let response = discovered(
            Vec::new(),
            vec![env(AwsCredentialType::BedrockApiKey, None)],
        );
        assert_eq!(
            method_list(&response),
            vec![
                Method::Environment,
                Method::ManualProfile,
                Method::AccessKeys,
                Method::ApiKey,
            ]
        );
    }

    #[test]
    fn method_list_with_nothing_detected_offers_manual_methods() {
        assert_eq!(
            method_list(&empty()),
            vec![Method::ManualProfile, Method::AccessKeys, Method::ApiKey]
        );
        let view = describe_method(Method::ManualProfile, &empty(), "default");
        assert_eq!(view.title, "AWS profile");
        assert_eq!(view.fields, MethodFields::ProfileName);
    }

    #[test]
    fn describes_profiles_and_environment_badges() {
        let response = discovered(
            vec![profile("work", Some("eu-west-1")), profile("other", None)],
            vec![
                env(AwsCredentialType::AccessKeys, Some("us-east-2")),
                env(AwsCredentialType::BedrockApiKey, Some("us-east-2")),
            ],
        );
        assert_eq!(
            describe_method(Method::Profile(0), &response, "work"),
            MethodView {
                title: "work".to_string(),
                detail: "AWS profile · eu-west-1".to_string(),
                badges: vec!["Active".to_string()],
                fields: MethodFields::None,
            }
        );
        assert_eq!(
            describe_method(Method::Profile(1), &response, "work").detail,
            "AWS profile · no region configured"
        );
        let environment = describe_method(Method::Environment, &response, "work");
        assert_eq!(
            environment.badges,
            vec!["Access keys".to_string(), "Bedrock API key".to_string()]
        );
        assert_eq!(
            environment.detail,
            "From AWS environment variables · us-east-2"
        );
        assert_eq!(
            describe_method(Method::ManualProfile, &response, "work").title,
            "Other AWS profile"
        );
    }

    #[test]
    fn selected_profile_defaults_to_default() {
        assert_eq!(selected_profile_name(None), "default");
        assert_eq!(selected_profile_name(Some("  ")), "default");
        assert_eq!(selected_profile_name(Some("dev")), "dev");
    }

    #[test]
    fn suggests_region_from_profile_or_environment() {
        let response = discovered(
            vec![profile("a", Some("eu-west-1")), profile("b", Some(" "))],
            vec![
                env(AwsCredentialType::AccessKeys, None),
                env(AwsCredentialType::BedrockApiKey, Some("us-west-2")),
            ],
        );
        assert_eq!(
            suggested_region(Method::Profile(0), &response),
            Some("eu-west-1".to_string())
        );
        assert_eq!(suggested_region(Method::Profile(1), &response), None);
        assert_eq!(
            suggested_region(Method::Environment, &response),
            Some("us-west-2".to_string())
        );
        assert_eq!(suggested_region(Method::ApiKey, &response), None);
    }

    #[test]
    fn validates_profile_methods() {
        let response = discovered(vec![profile("dev", Some("us-west-2"))], Vec::new());
        assert_eq!(
            validate_form(&form(Method::Profile(0)), context(&response)),
            Ok(SetupPlan {
                endpoint: Endpoint::Mantle,
                credential: Credential::Profile("dev".to_string()),
                region: "us-west-2".to_string(),
            })
        );
        let mut manual = form(Method::ManualProfile);
        assert_eq!(
            validate_form(&manual, context(&response)),
            Err(FormError::MissingProfile)
        );
        manual.manual_profile = "my sso".to_string();
        assert_eq!(
            validate_form(&manual, context(&response)),
            Err(FormError::InvalidProfile)
        );
        manual.manual_profile = " sso-admin ".to_string();
        assert_eq!(
            validate_form(&manual, context(&response)).map(|plan| plan.credential),
            Ok(Credential::Profile("sso-admin".to_string()))
        );
        let complete = FormContext {
            discovery_complete: true,
            ..context(&response)
        };
        assert_eq!(
            validate_form(&manual, complete),
            Err(FormError::UnknownProfile("sso-admin".to_string()))
        );
        manual.manual_profile = "dev".to_string();
        assert_eq!(
            validate_form(&manual, complete).map(|plan| plan.credential),
            Ok(Credential::Profile("dev".to_string()))
        );
    }

    #[test]
    fn validates_environment_method() {
        let without = empty();
        assert_eq!(
            validate_form(&form(Method::Environment), context(&without)),
            Err(FormError::NoEnvironmentCredentials)
        );
        let with = discovered(Vec::new(), vec![env(AwsCredentialType::AccessKeys, None)]);
        assert_eq!(
            validate_form(
                &form(Method::Environment),
                FormContext {
                    discovered: &with,
                    managed_credentials: true,
                    discovery_complete: true,
                }
            ),
            Err(FormError::ManagedCredentialsTakePriority)
        );
        assert_eq!(
            validate_form(&form(Method::Environment), context(&with)).map(|plan| plan.credential),
            Ok(Credential::Environment)
        );
    }

    #[test]
    fn validates_access_keys() {
        let response = empty();
        let mut input = form(Method::AccessKeys);
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::MissingAccessKeyId)
        );
        input.access_key_id = "AKIA-short".to_string();
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::InvalidAccessKeyId)
        );
        input.access_key_id = " AKIAIOSFODNN7EXAMPLE ".to_string();
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::MissingSecretAccessKey)
        );
        input.secret_access_key = "wJalr XUtnFEMI".to_string();
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::InvalidSecret)
        );
        input.secret_access_key = "wJalrXUtnFEMI/K7MDENG".to_string();
        assert_eq!(
            validate_form(&input, context(&response)).map(|plan| plan.credential),
            Ok(Credential::AccessKeys {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG".to_string(),
                session_token: None,
            })
        );
        input.session_token = "token".to_string();
        assert!(matches!(
            validate_form(&input, context(&response)).map(|plan| plan.credential),
            Ok(Credential::AccessKeys {
                session_token: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn validates_api_key_and_region() {
        let response = empty();
        let mut input = form(Method::ApiKey);
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::MissingApiKey)
        );
        input.api_key = "ABSK key".to_string();
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::InvalidSecret)
        );
        input.api_key = "ABSKexample".to_string();
        input.region = String::new();
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::MissingRegion)
        );
        input.region = "ap-southeast-2".to_string();
        assert_eq!(
            validate_form(&input, context(&response)),
            Err(FormError::UnsupportedRegion("ap-southeast-2".to_string()))
        );
        assert_eq!(
            FormError::UnsupportedRegion("ap-southeast-2".to_string()).to_string(),
            "Amazon Bedrock does not support region ap-southeast-2."
        );
        input.region = "us-gov-west-1".to_string();
        assert!(validate_form(&input, context(&response)).is_ok());
        assert_eq!(
            validate_form(&FormInput::default(), context(&response)),
            Err(FormError::NoMethod)
        );
    }

    #[test]
    fn credential_debug_redacts_secrets() {
        let debug = format!("{:?}", Credential::ApiKey("secret-value".to_string()));
        assert!(!debug.contains("secret-value"));
    }

    fn plan(endpoint: Endpoint, credential: Credential) -> SetupPlan {
        SetupPlan {
            endpoint,
            credential,
            region: "us-west-2".to_string(),
        }
    }

    fn edits_json(edits: &[ConfigEdit]) -> JsonValue {
        serde_json::to_value(edits).unwrap_or(JsonValue::Null)
    }

    #[test]
    fn mantle_profile_uses_setup_then_restarts() {
        let steps = plan_apply_steps(
            &plan(Endpoint::Mantle, Credential::Profile("dev".to_string())),
            "openai",
        );
        assert_eq!(
            steps,
            vec![
                Step::Setup(BedrockSetupParams::Profile {
                    profile: "dev".to_string(),
                    region: "us-west-2".to_string(),
                }),
                Step::ClearModel,
                Step::CheckGovCloud,
                Step::Restart,
                Step::LoadModels,
            ]
        );
    }

    #[test]
    fn mantle_reapply_keeps_the_chosen_model() {
        let steps = plan_apply_steps(
            &plan(Endpoint::Mantle, Credential::Environment),
            "amazon-bedrock",
        );
        assert_eq!(
            steps,
            vec![
                Step::Setup(BedrockSetupParams::Environment {
                    region: "us-west-2".to_string(),
                }),
                Step::CheckGovCloud,
                Step::Restart,
                Step::LoadModels,
            ]
        );
    }

    #[test]
    fn runtime_profile_validates_locally_and_writes_config() {
        let steps = plan_apply_steps(
            &plan(Endpoint::Runtime, Credential::Profile("dev".to_string())),
            "amazon-bedrock",
        );
        assert_eq!(
            steps[0],
            Step::ValidateProfile {
                profile: "dev".to_string(),
                region: "us-west-2".to_string(),
            }
        );
        let Step::Write(edits) = &steps[1] else {
            panic!("expected a config write, got {:?}", steps[1]);
        };
        assert_eq!(
            edits_json(edits),
            json!([
                {"keyPath": "model_provider", "value": "amazon-bedrock-runtime", "mergeStrategy": "replace"},
                {"keyPath": "model_providers.amazon-bedrock-runtime.aws.profile", "value": "dev", "mergeStrategy": "replace"},
                {"keyPath": "model_providers.amazon-bedrock-runtime.aws.region", "value": "us-west-2", "mergeStrategy": "replace"},
            ])
        );
        // Clearing the model is its own step: a layer that pins `model`
        // must not undo a provider change that already happened.
        assert_eq!(
            &steps[2..],
            &[
                Step::ClearModel,
                Step::CheckGovCloud,
                Step::Restart,
                Step::LoadModels
            ]
        );
        assert_eq!(
            edits_json(&clear_model_edits()),
            json!([{"keyPath": "model", "value": null, "mergeStrategy": "replace"}])
        );
    }

    #[test]
    fn runtime_with_stored_keys_logs_in_then_selects_runtime() {
        let steps = plan_apply_steps(
            &plan(Endpoint::Runtime, Credential::ApiKey("ABSK".to_string())),
            "amazon-bedrock-runtime",
        );
        assert_eq!(
            steps[0],
            Step::Login(LoginAccountParams::AmazonBedrock {
                api_key: "ABSK".to_string(),
                region: "us-west-2".to_string(),
            })
        );
        let Step::Write(edits) = &steps[1] else {
            panic!("expected a config write, got {:?}", steps[1]);
        };
        assert_eq!(
            edits_json(edits),
            json!([
                {"keyPath": "model_provider", "value": "amazon-bedrock-runtime", "mergeStrategy": "replace"},
                {"keyPath": "model_providers.amazon-bedrock-runtime.aws.profile", "value": null, "mergeStrategy": "replace"},
                {"keyPath": "model_providers.amazon-bedrock-runtime.aws.region", "value": "us-west-2", "mergeStrategy": "replace"},
            ])
        );
    }

    #[test]
    fn access_keys_login_carries_optional_session_token() {
        let steps = plan_apply_steps(
            &plan(
                Endpoint::Mantle,
                Credential::AccessKeys {
                    access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
                    secret_access_key: "secret".to_string(),
                    session_token: None,
                },
            ),
            "openai",
        );
        assert_eq!(
            serde_json::to_value(match &steps[0] {
                Step::Login(params) => params.clone(),
                other => panic!("expected login, got {other:?}"),
            })
            .unwrap_or(JsonValue::Null),
            json!({
                "type": "amazonBedrockAccessKeys",
                "accessKeyId": "AKIAIOSFODNN7EXAMPLE",
                "secretAccessKey": "secret",
                "sessionToken": null,
                "region": "us-west-2",
            })
        );
    }

    #[test]
    fn revert_clears_provider_model_and_aws_table() {
        assert_eq!(
            plan_revert_steps(
                "amazon-bedrock-runtime",
                /*remove_credentials*/ true,
                /*fallback_provider*/ None
            ),
            vec![
                Step::Logout,
                Step::Write(vec![
                    clear("model_provider"),
                    clear("model_providers.amazon-bedrock-runtime.aws"),
                ]),
                Step::ClearModel,
                Step::Restart,
            ]
        );
        assert_eq!(
            plan_revert_steps("ollama", /*remove_credentials*/ false, Some("openai")),
            vec![
                Step::Write(vec![clear("model_provider")]),
                Step::ClearModel,
                Step::Restart,
            ]
        );
    }

    #[test]
    fn revert_selects_openai_explicitly_over_a_lower_layer() {
        // Clearing the user's key would expose the system config's Bedrock.
        assert_eq!(
            plan_revert_steps(
                "amazon-bedrock",
                /*remove_credentials*/ false,
                Some("amazon-bedrock")
            ),
            vec![
                Step::Write(vec![
                    replace("model_provider", json!("openai")),
                    clear("model_providers.amazon-bedrock.aws"),
                ]),
                Step::ClearModel,
                Step::Restart,
            ]
        );
    }

    fn layer(source: ConfigLayerSource, config: JsonValue, disabled: bool) -> ConfigLayer {
        ConfigLayer {
            name: source,
            version: "sha256:0".to_string(),
            config,
            disabled_reason: disabled.then(|| "untrusted".to_string()),
        }
    }

    #[test]
    fn fallback_provider_comes_from_the_highest_lower_layer() {
        let user = ConfigLayerSource::User {
            file: abs("/home/u/.codex/config.toml"),
            profile: None,
        };
        let system = ConfigLayerSource::System {
            file: abs("/etc/codex/config.toml"),
        };
        let mdm = ConfigLayerSource::Mdm {
            domain: "com.openai.codex".to_string(),
            key: "config".to_string(),
        };
        let mut response = config_read(
            json!({"model_provider": "amazon-bedrock"}),
            Some(user.clone()),
        );
        response.layers = Some(vec![
            layer(user, json!({"model_provider": "amazon-bedrock"}), false),
            layer(system, json!({"model_provider": "amazon-bedrock"}), false),
            layer(mdm, json!({"model_provider": "openai"}), false),
        ]);
        let status = provider_status(&response);
        assert_eq!(status.provider_origin, None, "the user config wins today");
        assert_eq!(status.fallback_provider, Some("amazon-bedrock".to_string()));

        let project = ConfigLayerSource::Project {
            dot_codex_folder: abs("/r/.codex"),
        };
        assert_eq!(
            fallback_provider(&[
                layer(project, json!({"model_provider": "corp"}), false),
                layer(
                    ConfigLayerSource::PackagedDefaults {
                        file: abs("/opt/codex/defaults.toml")
                    },
                    json!({"model_provider": "corp"}),
                    true
                ),
            ]),
            None,
            "higher and disabled layers do not count"
        );
        assert_eq!(
            provider_status(&config_read(json!({}), None)).fallback_provider,
            None
        );
    }

    #[test]
    fn warnings_follow_the_success_message() {
        let mut flow = Flow::new(1, FlowKind::Revert, vec![Step::ClearModel, Step::Restart]);
        flow.warn("The model is still set by a profile.".to_string());
        assert_eq!(
            flow.finished_message("Codex uses OpenAI again.".to_string()),
            "Codex uses OpenAI again. The model is still set by a profile."
        );
    }

    #[test]
    fn local_steps_select_provider_and_model() {
        assert_eq!(
            plan_local_steps("ollama", "gpt-oss:20b"),
            vec![
                Step::Write(vec![
                    replace("model_provider", json!("ollama")),
                    replace("model", json!("gpt-oss:20b")),
                ]),
                Step::Restart,
            ]
        );
        assert_eq!(
            edits_json(&model_edits("openai.gpt-5.6-luna")),
            json!([{"keyPath": "model", "value": "openai.gpt-5.6-luna", "mergeStrategy": "replace"}])
        );
    }

    #[test]
    fn remote_servers_skip_local_validation_and_restart() {
        let steps = plan_apply_steps(
            &plan(Endpoint::Runtime, Credential::Profile("dev".to_string())),
            "openai",
        );
        let remote = without_embedded_steps(steps);
        assert_eq!(remote.len(), 3);
        assert!(matches!(remote[0], Step::Write(_)));
        assert_eq!(remote[1..], [Step::ClearModel, Step::CheckGovCloud]);
        assert_eq!(
            without_embedded_steps(plan_local_steps("ollama", "qwen3:8b")).len(),
            1
        );
    }

    #[test]
    fn flow_runs_steps_in_order() {
        let mut flow = Flow::new(
            7,
            FlowKind::Revert,
            plan_revert_steps(
                "amazon-bedrock",
                /*remove_credentials*/ false,
                /*fallback_provider*/ None,
            ),
        );
        assert_eq!(flow.current(), None);
        assert!(matches!(flow.advance(), Some(Step::Write(_))));
        assert_eq!(flow.advance(), Some(Step::ClearModel));
        assert_eq!(flow.advance(), Some(Step::Restart));
        assert_eq!(flow.current(), Some(&Step::Restart));
        assert_eq!(flow.advance(), None);
        assert_eq!(flow.current(), None);
    }

    #[test]
    fn postponing_the_restart_ends_the_flow() {
        let mut flow = Flow::new(
            1,
            FlowKind::Bedrock {
                endpoint: Endpoint::Mantle,
                region: "us-east-1".to_string(),
            },
            plan_apply_steps(&plan(Endpoint::Mantle, Credential::Environment), "openai"),
        );
        while flow.advance() != Some(Step::Restart) {}
        flow.skip_restart();
        assert_eq!(flow.current(), None);
        assert_eq!(flow.advance(), None);
    }

    #[test]
    fn only_local_validation_is_cancellable() {
        assert!(
            Step::ValidateProfile {
                profile: "p".to_string(),
                region: "us-east-1".to_string(),
            }
            .is_cancellable()
        );
        assert!(
            !Step::Setup(BedrockSetupParams::Environment {
                region: "us-east-1".to_string(),
            })
            .is_cancellable()
        );
        assert!(!Step::Restart.is_cancellable());
        assert_eq!(
            Step::CheckGovCloud.progress_label(),
            "Checking AWS GovCloud requirements…"
        );
    }

    fn config_read(config: JsonValue, origin: Option<ConfigLayerSource>) -> ConfigReadResponse {
        let mut origins = HashMap::new();
        if let Some(source) = origin {
            origins.insert(
                "model_provider".to_string(),
                ConfigLayerMetadata {
                    name: source,
                    version: "sha256:0".to_string(),
                },
            );
        }
        ConfigReadResponse {
            config: serde_json::from_value(config).unwrap_or_else(|err| panic!("config: {err}")),
            origins,
            layers: None,
        }
    }

    fn abs(path: &str) -> codex_utils_absolute_path::AbsolutePathBuf {
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(path)
            .unwrap_or_else(|err| panic!("absolute path: {err}"))
    }

    #[test]
    fn reads_provider_status_from_config() {
        let response = config_read(
            json!({
                "model": "openai.gpt-5.6-luna",
                "model_provider": "amazon-bedrock",
                "model_providers": {
                    "amazon-bedrock": {"aws": {"profile": "dev", "region": "us-west-2"}}
                }
            }),
            Some(ConfigLayerSource::User {
                file: abs("/home/u/.codex/config.toml"),
                profile: None,
            }),
        );
        assert_eq!(
            provider_status(&response),
            ProviderStatus {
                provider_id: "amazon-bedrock".to_string(),
                provider_name: None,
                model: Some("openai.gpt-5.6-luna".to_string()),
                aws_profile: Some("dev".to_string()),
                aws_region: Some("us-west-2".to_string()),
                provider_origin: None,
                provider_locked: false,
                fallback_provider: None,
            }
        );
    }

    #[test]
    fn unset_provider_means_openai_and_overrides_lock_the_page() {
        let status = provider_status(&config_read(json!({}), None));
        assert_eq!(status.provider_id, "openai");
        assert_eq!(status.model, None);

        let locked = provider_status(&config_read(
            json!({"model_provider": "mock", "model_providers": {"mock": {"name": "Mock"}}}),
            Some(ConfigLayerSource::SessionFlags),
        ));
        assert_eq!(locked.provider_name, Some("Mock".to_string()));
        assert_eq!(
            locked.provider_origin,
            Some("command-line flags".to_string())
        );
        assert!(locked.provider_locked);

        let system = provider_status(&config_read(
            json!({"model_provider": "amazon-bedrock"}),
            Some(ConfigLayerSource::System {
                file: abs("/etc/codex/config.toml"),
            }),
        ));
        assert_eq!(
            system.provider_origin,
            Some("system config /etc/codex/config.toml".to_string())
        );
        assert!(!system.provider_locked);
    }

    #[test]
    fn labels_providers_and_accounts() {
        assert_eq!(provider_label("openai", None), "OpenAI");
        assert_eq!(
            provider_label("amazon-bedrock-runtime", None),
            "Amazon Bedrock (Runtime)"
        );
        assert_eq!(provider_label("mock", Some("Mock")), "Mock (mock)");
        assert_eq!(provider_label("custom", None), "custom");
        assert_eq!(
            account_label(
                &GetAccountResponse {
                    account: Some(Account::AmazonBedrock {
                        uses_codex_managed_credentials: true,
                    }),
                    requires_openai_auth: false,
                    workspace_routing: None,
                },
                /*aws_profile*/ None
            ),
            "Amazon Bedrock credentials stored by Codex"
        );
        let aws = GetAccountResponse {
            account: Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: false,
            }),
            requires_openai_auth: false,
            workspace_routing: None,
        };
        assert_eq!(
            account_label(&aws, Some("dev")),
            "Credentials from AWS profile \"dev\""
        );
        assert_eq!(
            account_label(&aws, /*aws_profile*/ None),
            "AWS credentials from the environment or the AWS SDK chain"
        );
        let signed_out = GetAccountResponse {
            account: None,
            requires_openai_auth: true,
            workspace_routing: None,
        };
        assert_eq!(
            account_label(&signed_out, /*aws_profile*/ None),
            "Not signed in"
        );
        assert!(!uses_managed_credentials(Some(&signed_out)));
        assert!(!uses_managed_credentials(None));
    }

    #[test]
    fn flow_messages_name_the_change() {
        assert_eq!(
            FlowKind::Local {
                provider_id: "ollama".to_string(),
                model: "gpt-oss:20b".to_string(),
            }
            .success_message(),
            "New threads use gpt-oss:20b on Ollama."
        );
        assert_eq!(
            FlowKind::Bedrock {
                endpoint: Endpoint::Runtime,
                region: "us-west-2".to_string(),
            }
            .success_message(),
            "Amazon Bedrock (Runtime) is set up in us-west-2. New threads use it."
        );
    }
}
