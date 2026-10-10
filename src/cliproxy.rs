//! `CLIProxyAPI` v8 management transport. Go config discovery briefly receives API keys;
//! unknown secret fields are discarded during deserialization, never stored or forwarded.
use std::{collections::HashMap, env, time::Duration};

use chrono::Utc;
use reqwest::{StatusCode, Url, blocking::Client, header::HeaderValue, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    CLAUDE_CODE_RATE_LIMIT_COOLDOWN, CLAUDE_CODE_REFRESH_INTERVAL, CLAUDE_CODE_USAGE_URL,
    CODEX_USAGE_URL, ClaudeCodeCache, ClaudeCodeCooldown, OPENCODE_GO_USAGE_URL, Provider,
    ProviderUsage, cached_claude_code_usage, claude_code_retry_after, claude_code_windows,
    codex_windows, opencode_api_windows, provider_status,
};

const SOURCE: &str = "cliproxy";
const URL_ENV: &str = "LLM_USAGE_CLIPROXY_URL";
const KEY_ENV: &str = "LLM_USAGE_CLIPROXY_MANAGEMENT_KEY";
const CODEX_INDEX_ENV: &str = "LLM_USAGE_CLIPROXY_CODEX_AUTH_INDEX";
const CLAUDE_INDEX_ENV: &str = "LLM_USAGE_CLIPROXY_CLAUDE_AUTH_INDEX";
const GO_INDEX_ENV: &str = "LLM_USAGE_CLIPROXY_OPENCODE_GO_AUTH_INDEX";
const GO_PROVIDER_ENV: &str = "LLM_USAGE_CLIPROXY_OPENCODE_GO_PROVIDER";
const ACCOUNT_ENV: &str = "LLM_USAGE_CLIPROXY_CODEX_ACCOUNT_ID";
const INVALID_RESPONSE: &str = "CLIProxyAPI management response was invalid";
const GO_CONFIG_PATH: &str = "config/api-keys/openai-compatibility";
const GO_DISCOVERY_INTERVAL: u64 = 300;
const GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

// Deliberately omit api-key, headers, proxy-url, and arbitrary server text. Serde
// ignores those fields instead of materializing them in a retained Value/string.
#[derive(Deserialize)]
struct GoConfigGroup {
    name: String,
    #[serde(rename = "base-url")]
    base_url: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    keys: Vec<GoConfigKey>,
}

#[derive(Deserialize)]
struct GoConfigKey {
    #[serde(default, alias = "auth-index", alias = "authIndex")]
    auth_index: Option<String>,
    #[serde(default)]
    disabled: bool,
}

struct GoDiscovery {
    refresh_after: u64,
    // Only sanitized metadata or constant, redacted failure messages survive.
    result: Result<Vec<Value>, Failure>,
}

pub(super) struct Session {
    client: Client,
    base: Url,
    authorization: HeaderValue,
    codex_index: Option<String>,
    claude_index: Option<String>,
    go_index: Option<String>,
    go_provider: String,
    codex_account: Option<String>,
    // Isolated per session/server and remote credential identity, never written to disk.
    claude_cache: HashMap<String, ClaudeCodeCache>,
    last_claude_key: Option<String>,
    discovery_failure: Option<(u64, Failure)>,
    go_discovery: Option<GoDiscovery>,
}

#[derive(Clone)]
struct Failure {
    message: &'static str,
    authentication: bool,
    delay: u64,
    cooldown: ClaudeCodeCooldown,
}

impl Failure {
    fn transient(message: &'static str) -> Self {
        Self {
            message,
            authentication: false,
            delay: CLAUDE_CODE_REFRESH_INTERVAL,
            cooldown: ClaudeCodeCooldown::Transient,
        }
    }

    fn authentication(message: &'static str) -> Self {
        Self {
            authentication: true,
            ..Self::transient(message)
        }
    }

    fn rate_limited(headers: &Value) -> Self {
        let value = headers
            .as_object()
            .and_then(|headers| {
                headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                    .and_then(|(_, value)| value.as_str().or_else(|| value.get(0)?.as_str()))
            })
            .and_then(|value| HeaderValue::from_str(value).ok());
        Self::rate_limited_header(value.as_ref())
    }

    fn rate_limited_header(value: Option<&HeaderValue>) -> Self {
        Self {
            message: "CLIProxyAPI quota request was rate limited (HTTP 429)",
            authentication: false,
            delay: claude_code_retry_after(value, Utc::now())
                .unwrap_or(CLAUDE_CODE_RATE_LIMIT_COOLDOWN)
                .max(60),
            cooldown: ClaudeCodeCooldown::RateLimited,
        }
    }
}

pub(super) fn unavailable(provider: Provider, message: &str, fetched_at: u64) -> ProviderUsage {
    let mut usage = super::unavailable(provider, message, fetched_at);
    usage.source = Some(SOURCE);
    usage
}

fn setting(name: &str) -> Option<String> {
    setting_with(name, |name| env::var(name).ok())
}

fn setting_with(name: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    let work_name = format!("WORK_{name}");
    [name, &work_name].into_iter().find_map(|name| {
        lookup(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

impl Session {
    pub(super) fn from_env() -> Result<Self, &'static str> {
        let url = setting(URL_ENV).ok_or("Set LLM_USAGE_CLIPROXY_URL for CLIProxyAPI mode")?;
        let key =
            setting(KEY_ENV).ok_or("Set LLM_USAGE_CLIPROXY_MANAGEMENT_KEY for CLIProxyAPI mode")?;
        Self::new(
            &url,
            &key,
            setting(CODEX_INDEX_ENV),
            setting(CLAUDE_INDEX_ENV),
            setting(GO_INDEX_ENV),
            setting(GO_PROVIDER_ENV),
            setting(ACCOUNT_ENV),
        )
    }

    fn new(
        url: &str,
        key: &str,
        codex_index: Option<String>,
        claude_index: Option<String>,
        go_index: Option<String>,
        go_provider: Option<String>,
        codex_account: Option<String>,
    ) -> Result<Self, &'static str> {
        let mut base = Url::parse(url).map_err(|_| "CLIProxyAPI URL was invalid")?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !matches!(base.path().trim_end_matches('/'), "" | "/v8/management")
        {
            return Err(
                "CLIProxyAPI URL must be an HTTP(S) server base or /v8/management URL, without credentials, query or fragment",
            );
        }
        base.set_path("/v8/management/");
        if key.trim().is_empty() {
            return Err("CLIProxyAPI management key was empty");
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| "CLIProxyAPI management key was invalid")?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(Policy::none())
            .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "CLIProxyAPI HTTP client could not be created")?;
        Ok(Self {
            client,
            base,
            authorization,
            codex_index,
            claude_index,
            go_index,
            go_provider: go_provider.unwrap_or_else(|| Provider::OpencodeGo.name().to_owned()),
            codex_account,
            claude_cache: HashMap::new(),
            last_claude_key: None,
            discovery_failure: None,
            go_discovery: None,
        })
    }

    pub(super) fn fetch(&mut self, providers: &[Provider], fetched_at: u64) -> Vec<ProviderUsage> {
        let files = if providers.is_empty() {
            Ok(Vec::new())
        } else if let Some((retry_at, failure)) = &self.discovery_failure
            && *retry_at > fetched_at
        {
            Err(failure.clone())
        } else {
            let result = self.credentials();
            self.discovery_failure = result
                .as_ref()
                .err()
                .map(|failure| (fetched_at.saturating_add(failure.delay), failure.clone()));
            result
        };
        if let Err(failure) = &files
            && failure.authentication
        {
            self.claude_cache.clear();
            self.last_claude_key = None;
            self.go_discovery = None;
        }
        providers
            .iter()
            .map(|&provider| {
                let files = match &files {
                    Ok(files) => files,
                    Err(failure) => {
                        if matches!(provider, Provider::ClaudeCode)
                            && let Some(key) = self.last_claude_key.clone()
                        {
                            return self.failed_claude_refresh(&key, failure, fetched_at);
                        }
                        return unavailable(provider, failure.message, fetched_at);
                    }
                };
                if matches!(provider, Provider::OpencodeGo) {
                    return self.fetch_go(files, fetched_at);
                }
                let selector = match provider {
                    Provider::Codex => self.codex_index.as_deref(),
                    Provider::ClaudeCode => self.claude_index.as_deref(),
                    Provider::OpencodeGo => self.go_index.as_deref(),
                };
                let (file, index) =
                    match select_credential(files, provider, selector, &self.go_provider) {
                        Ok(credential) => credential,
                        Err(message) => {
                            if matches!(provider, Provider::ClaudeCode) {
                                self.last_claude_key = None;
                            }
                            return unavailable(provider, message, fetched_at);
                        }
                    };
                self.fetch_provider(provider, file, &index, fetched_at)
            })
            .collect()
    }

    fn fetch_go(&mut self, files: &[Value], fetched_at: u64) -> ProviderUsage {
        if let Some(usage) = self.fetch_hidden_go(files, fetched_at) {
            return usage;
        }
        let mut candidates = files.to_vec();
        // Avoid receiving config secrets when ordinary metadata (or an explicit
        // index) suffices. Never use config to bypass visible group/disabled checks.
        if self.go_index.is_none()
            && !files
                .iter()
                .any(|file| matches_go_provider(file, &self.go_provider))
        {
            let discovered = match self.go_credentials(fetched_at) {
                Ok(discovered) => discovered,
                Err(failure) => {
                    return unavailable(Provider::OpencodeGo, failure.message, fetched_at);
                }
            };
            for file in discovered {
                let index = identifier(file.get("auth_index"));
                if candidates.iter().any(|listed| {
                    identifier(listed.get("auth_index").or_else(|| listed.get("authIndex")))
                        == index
                }) {
                    return unavailable(
                        Provider::OpencodeGo,
                        "CLIProxyAPI Go config auth_index conflicts with visible credential metadata; refusing quota request",
                        fetched_at,
                    );
                }
                candidates.push(file);
            }
        }
        match select_credential(
            &candidates,
            Provider::OpencodeGo,
            self.go_index.as_deref(),
            &self.go_provider,
        ) {
            Ok((file, index)) => {
                self.fetch_provider(Provider::OpencodeGo, file, &index, fetched_at)
            }
            Err(message) => unavailable(Provider::OpencodeGo, message, fetched_at),
        }
    }

    fn go_credentials(&mut self, fetched_at: u64) -> Result<Vec<Value>, Failure> {
        if let Some(cache) = &self.go_discovery
            && cache.refresh_after > fetched_at
        {
            return cache.result.clone();
        }
        let result = self.discover_go_credentials();
        let delay = result
            .as_ref()
            .map_or_else(|failure| failure.delay, |_| GO_DISCOVERY_INTERVAL);
        self.go_discovery = Some(GoDiscovery {
            refresh_after: fetched_at.saturating_add(delay),
            result: result.clone(),
        });
        result
    }

    fn discover_go_credentials(&self) -> Result<Vec<Value>, Failure> {
        let response = self
            .client
            .get(self.base.join(GO_CONFIG_PATH).expect("fixed config path"))
            .header("Authorization", self.authorization.clone())
            .send()
            .map_err(|_| Failure::transient("CLIProxyAPI Go configuration discovery failed"))?;
        check_management_response(&response)?;
        // The HTTP body transiently contains raw keys. Only these typed,
        // non-secret fields survive parsing; never log or cache the body.
        let groups: Vec<GoConfigGroup> = response
            .json()
            .map_err(|_| Failure::transient(INVALID_RESPONSE))?;
        sanitized_go_credentials(groups, &self.go_provider)
    }

    // An explicit hidden index remains an operator-approved Go reference and
    // bypasses config reads. Listed indexes must still pass provider/disabled checks.
    fn fetch_hidden_go(&mut self, files: &[Value], fetched_at: u64) -> Option<ProviderUsage> {
        let index = self.go_index.clone()?;
        if files.iter().any(|file| {
            identifier(file.get("auth_index").or_else(|| file.get("authIndex")))
                .is_some_and(|listed| listed == index)
        }) {
            return None;
        }
        // Stable indexes in v8 are the first eight SHA-256 bytes, hex-encoded.
        // Do not accept an API key or arbitrary server identifier by mistake.
        if index.len() != 16 || !index.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(unavailable(
                Provider::OpencodeGo,
                "Hidden CLIProxyAPI Go credentials require a 16-character hexadecimal auth_index, not an API key",
                fetched_at,
            ));
        }
        Some(self.fetch_provider(Provider::OpencodeGo, &Value::Null, &index, fetched_at))
    }

    fn credentials(&self) -> Result<Vec<Value>, Failure> {
        let response = self
            .client
            .get(self.base.join("credentials").expect("fixed API path"))
            .header("Authorization", self.authorization.clone())
            .send()
            .map_err(|_| Failure::transient("CLIProxyAPI credential discovery failed"))?;
        check_management_response(&response)?;
        let data: Value = response
            .json()
            .map_err(|_| Failure::transient(INVALID_RESPONSE))?;
        data.get("files")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| Failure::transient(INVALID_RESPONSE))
    }

    fn request_usage(
        &self,
        provider: Provider,
        file: &Value,
        index: &str,
    ) -> Result<Value, Failure> {
        let mut header = json!({"Authorization": "Bearer $TOKEN$", "Accept": "application/json"});
        let url = match provider {
            Provider::Codex => {
                if let Some(account) = self.codex_account.clone().or_else(|| account_id(file)) {
                    header["ChatGPT-Account-Id"] = json!(account);
                }
                CODEX_USAGE_URL
            }
            Provider::ClaudeCode => {
                header["anthropic-beta"] = json!("oauth-2025-04-20");
                CLAUDE_CODE_USAGE_URL
            }
            Provider::OpencodeGo => OPENCODE_GO_USAGE_URL,
        };
        let response = self
            .client
            .post(self.base.join("requests/api-call").expect("fixed API path"))
            .header("Authorization", self.authorization.clone())
            .json(&json!({"authIndex": index, "method": "GET", "url": url, "header": header}))
            .send()
            .map_err(|_| Failure::transient("CLIProxyAPI quota request failed"))?;
        if matches!(provider, Provider::OpencodeGo) && response.status() == StatusCode::BAD_REQUEST
        {
            return Err(Failure::transient(
                "CLIProxyAPI Go quota request was rejected; check the configured Go auth_index and server-held key",
            ));
        }
        check_management_response(&response)?;
        let envelope: Value = response
            .json()
            .map_err(|_| Failure::transient(INVALID_RESPONSE))?;
        parse_envelope(&envelope)
    }

    fn fetch_provider(
        &mut self,
        provider: Provider,
        file: &Value,
        index: &str,
        fetched_at: u64,
    ) -> ProviderUsage {
        let cache_key = credential_identity(file, index);
        if matches!(provider, Provider::ClaudeCode) {
            self.last_claude_key = Some(cache_key.clone());
        }
        if matches!(provider, Provider::ClaudeCode)
            && let Some(cache) = self.claude_cache.get(&cache_key)
            && cache.refresh_after > fetched_at
        {
            let remaining = cache.refresh_after - fetched_at;
            let reason = if cache.cooldown.is_some() {
                "CLIProxyAPI Claude quota refresh is in cooldown"
            } else {
                "CLIProxyAPI Claude quota refresh is throttled"
            };
            let message = format!("{reason}; retry in {}", super::short_duration(remaining));
            return remote_cached_usage(cache, fetched_at, &message)
                .unwrap_or_else(|| unavailable(provider, &message, fetched_at));
        }
        let result = self
            .request_usage(provider, file, index)
            .and_then(|data| usage_from_data(provider, &data, fetched_at));
        match result {
            Ok(usage) => {
                if matches!(provider, Provider::ClaudeCode) {
                    self.claude_cache.insert(
                        cache_key.clone(),
                        ClaudeCodeCache::from_usage(
                            &usage,
                            fetched_at.saturating_add(CLAUDE_CODE_REFRESH_INTERVAL),
                        ),
                    );
                }
                usage
            }
            Err(failure) => {
                if matches!(provider, Provider::ClaudeCode) {
                    return self.failed_claude_refresh(&cache_key, &failure, fetched_at);
                }
                unavailable(provider, failure.message, fetched_at)
            }
        }
    }

    fn failed_claude_refresh(
        &mut self,
        key: &str,
        failure: &Failure,
        fetched_at: u64,
    ) -> ProviderUsage {
        if failure.authentication {
            self.claude_cache.remove(key);
            return unavailable(Provider::ClaudeCode, failure.message, fetched_at);
        }
        let cache = self
            .claude_cache
            .entry(key.to_owned())
            .or_insert_with(ClaudeCodeCache::empty);
        cache.refresh_after = fetched_at.saturating_add(failure.delay);
        cache.cooldown = Some(failure.cooldown);
        let message = format!(
            "{}; retry in {}",
            failure.message,
            super::short_duration(failure.delay)
        );
        remote_cached_usage(cache, fetched_at, &message)
            .unwrap_or_else(|| unavailable(Provider::ClaudeCode, &message, fetched_at))
    }
}

fn remote_cached_usage(
    cache: &ClaudeCodeCache,
    fetched_at: u64,
    message: &str,
) -> Option<ProviderUsage> {
    let mut usage = cached_claude_code_usage(cache, fetched_at, message)?;
    usage.source = Some(SOURCE);
    // Normal five-minute refresh throttling is not a failed refresh.
    if cache.cooldown.is_none() {
        usage.status = provider_status(&usage.windows);
        usage.error = None;
    }
    Some(usage)
}

fn check_management_response(response: &reqwest::blocking::Response) -> Result<(), Failure> {
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let mut failure =
            Failure::rate_limited_header(response.headers().get(reqwest::header::RETRY_AFTER));
        failure.message = "CLIProxyAPI management request was rate limited (HTTP 429)";
        return Err(failure);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        Err(Failure::authentication(
            "CLIProxyAPI management key was rejected",
        ))
    } else if status.is_success() {
        Ok(())
    } else if status == StatusCode::NOT_FOUND {
        Err(Failure::transient(
            "CLIProxyAPI v8 management endpoint was not found",
        ))
    } else {
        Err(Failure::transient("CLIProxyAPI management request failed"))
    }
}

fn parse_envelope(envelope: &Value) -> Result<Value, Failure> {
    let status = envelope
        .get("status_code")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .and_then(|status| StatusCode::from_u16(status).ok())
        .ok_or_else(|| Failure::transient(INVALID_RESPONSE))?;
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(Failure::authentication(
            "CLIProxyAPI remote provider credential was rejected",
        ));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(Failure::rate_limited(&envelope["header"]));
    }
    if !status.is_success() {
        return Err(Failure::transient(
            "CLIProxyAPI upstream quota request failed",
        ));
    }
    let body = envelope
        .get("body")
        .ok_or_else(|| Failure::transient(INVALID_RESPONSE))?;
    let data = match body {
        Value::String(body) => {
            serde_json::from_str(body).map_err(|_| Failure::transient(INVALID_RESPONSE))?
        }
        body => body.clone(),
    };
    if !data.is_object() {
        return Err(Failure::transient(INVALID_RESPONSE));
    }
    Ok(data)
}

fn usage_from_data(
    provider: Provider,
    data: &Value,
    fetched_at: u64,
) -> Result<ProviderUsage, Failure> {
    let windows = match provider {
        Provider::Codex => codex_windows(data),
        Provider::ClaudeCode => claude_code_windows(data),
        Provider::OpencodeGo => opencode_api_windows(data),
    };
    // The existing parsers may retain an unavailable 5h placeholder even for malformed data.
    if !windows.iter().any(|window| window.used_percent.is_some()) {
        return Err(Failure::transient(
            "CLIProxyAPI provider usage windows unavailable",
        ));
    }
    let plan = data
        .get("plan_type")
        .or_else(|| data.get("plan"))
        .and_then(Value::as_str)
        // Keep arbitrary server text (and any echoed credential) out of public output/cache.
        .filter(|plan| {
            matches!(
                *plan,
                "free" | "plus" | "pro" | "team" | "enterprise" | "edu" | "max" | "max5" | "max20"
            )
        })
        .map(ToOwned::to_owned);
    Ok(ProviderUsage {
        provider: provider.name(),
        status: provider_status(&windows),
        available: true,
        plan: if matches!(provider, Provider::OpencodeGo) {
            Some("Go".to_owned())
        } else {
            plan
        },
        source: Some(SOURCE),
        windows,
        error: None,
        fetched_at,
    })
}

fn sanitized_go_credentials(
    groups: Vec<GoConfigGroup>,
    expected: &str,
) -> Result<Vec<Value>, Failure> {
    let expected = expected.trim().to_ascii_lowercase();
    let expected = expected
        .strip_prefix("openai-compatible-")
        .unwrap_or(&expected);
    let mut credentials = Vec::new();
    for group in groups {
        if group.disabled || !group.name.trim().eq_ignore_ascii_case(expected) {
            continue;
        }
        let upstream = Url::parse(group.base_url.trim()).map_err(|_| {
            Failure::transient("CLIProxyAPI Go provider has an invalid upstream URL")
        })?;
        if upstream.scheme() != "https"
            || upstream.host_str() != Some("opencode.ai")
            || upstream.port_or_known_default() != Some(443)
            || upstream.path().trim_end_matches('/') != "/zen/go/v1"
            || !upstream.username().is_empty()
            || upstream.password().is_some()
            || upstream.query().is_some()
            || upstream.fragment().is_some()
        {
            return Err(Failure::transient(
                "CLIProxyAPI Go provider upstream must be https://opencode.ai/zen/go/v1",
            ));
        }
        for key in group.keys.into_iter().filter(|key| !key.disabled) {
            let index = key
                .auth_index
                .filter(|index| {
                    index.len() == 16 && index.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| {
                    Failure::transient(
                        "CLIProxyAPI Go configuration lacks a valid per-key auth_index",
                    )
                })?;
            if credentials
                .iter()
                .any(|file: &Value| file["auth_index"] == index)
            {
                return Err(Failure::transient(
                    "CLIProxyAPI Go configuration contains duplicate auth_index values; refusing quota request",
                ));
            }
            credentials.push(json!({"provider": format!("openai-compatible-{expected}"),
                "label": expected, "base_url": GO_BASE_URL, "auth_index": index, "disabled": false}));
        }
    }
    Ok(credentials)
}

fn identifier(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.trim().to_owned()).filter(|value| !value.is_empty()),
        Value::Number(value) if value.is_u64() => Some(value.to_string()),
        _ => None,
    }
}

// v8 OpenAI-compatible groups expose their internal provider key and label, not their
// base URL. Only an exact Go group match is safe: never select a generic compatible
// key (or an unrelated OAuth account) just because it has the requested auth index.
// Hidden indexes are handled separately as an explicit operator-approved Go reference.
fn matches_go_provider(file: &Value, expected: &str) -> bool {
    let expected = expected.trim().to_ascii_lowercase();
    let expected = expected
        .strip_prefix("openai-compatible-")
        .unwrap_or(&expected);
    if expected.is_empty() {
        return false;
    }
    [file.get("type"), file.get("provider")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|name| {
            let name = name.trim().to_ascii_lowercase();
            let group = name.strip_prefix("openai-compatible-").unwrap_or(&name);
            (group == expected && (name.starts_with("openai-compatible-") || name == "opencode-go"))
                || (name == "openai-compatibility"
                    && file
                        .get("label")
                        .and_then(Value::as_str)
                        .is_some_and(|label| label.trim().eq_ignore_ascii_case(expected)))
        })
}

fn select_credential<'a>(
    files: &'a [Value],
    provider: Provider,
    selector: Option<&str>,
    go_provider: &str,
) -> Result<(&'a Value, String), &'static str> {
    let candidates = files
        .iter()
        .filter(|file| {
            let disabled = match file.get("disabled") {
                Some(Value::Bool(value)) => *value,
                Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
                Some(Value::String(value)) => {
                    value.trim().eq_ignore_ascii_case("true") || value.trim() == "1"
                }
                _ => false,
            };
            if disabled
                || file
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status.trim().eq_ignore_ascii_case("disabled"))
            {
                return false;
            }
            if matches!(provider, Provider::OpencodeGo) {
                return matches_go_provider(file, go_provider);
            }
            [file.get("type"), file.get("provider")]
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|name| match provider {
                    Provider::Codex => name.eq_ignore_ascii_case("codex"),
                    Provider::ClaudeCode => {
                        name.eq_ignore_ascii_case("claude")
                            || name.eq_ignore_ascii_case("anthropic")
                    }
                    Provider::OpencodeGo => false,
                })
        })
        .filter_map(|file| {
            identifier(file.get("auth_index").or_else(|| file.get("authIndex")))
                .map(|index| (file, index))
        })
        .filter(|(_, index)| selector.is_none_or(|selector| selector == index))
        .collect::<Vec<_>>();
    if matches!(provider, Provider::OpencodeGo)
        && candidates.iter().any(|(_, index)| {
            files
                .iter()
                .filter(|file| {
                    identifier(file.get("auth_index").or_else(|| file.get("authIndex")))
                        .is_some_and(|listed| listed == *index)
                })
                .count()
                > 1
        })
    {
        return Err(
            "Conflicting CLIProxyAPI Go credential metadata shares an auth_index; refusing quota request",
        );
    }
    match candidates.len() {
        0 if matches!(provider, Provider::OpencodeGo) => Err(
            "No matching enabled CLIProxyAPI Go key with an auth_index; check the configured group and LLM_USAGE_CLIPROXY_OPENCODE_GO_PROVIDER",
        ),
        0 => Err(
            "No matching enabled CLIProxyAPI credential with an auth index; check the provider and account selector",
        ),
        1 => Ok(candidates.into_iter().next().expect("one credential")),
        _ => Err(match provider {
            Provider::Codex => {
                "Multiple CLIProxyAPI Codex credentials; set LLM_USAGE_CLIPROXY_CODEX_AUTH_INDEX"
            }
            Provider::ClaudeCode => {
                "Multiple CLIProxyAPI Claude credentials; set LLM_USAGE_CLIPROXY_CLAUDE_AUTH_INDEX"
            }
            Provider::OpencodeGo => {
                "Multiple CLIProxyAPI OpenCode Go credentials; set LLM_USAGE_CLIPROXY_OPENCODE_GO_AUTH_INDEX"
            }
        }),
    }
}

// Only explicitly non-secret identity/revision fields participate in cache isolation.
fn credential_identity(file: &Value, index: &str) -> String {
    let mut identity = vec![
        json!(index),
        file.get("name").cloned().unwrap_or_default(),
        json!(account_id(file)),
    ];
    for root in [Some(file), file.get("metadata"), file.get("attributes")]
        .into_iter()
        .flatten()
    {
        for key in [
            "id",
            "email",
            "account_id",
            "accountId",
            "account_uuid",
            "user_id",
            "userId",
            "organization_id",
            "organization_uuid",
            "modified",
            "modified_at",
            "updated_at",
            "last_refresh",
        ] {
            identity.push(json!([key, root.get(key)]));
        }
    }
    Value::Array(identity).to_string()
}

fn account_id(file: &Value) -> Option<String> {
    for root in [Some(file), file.get("metadata"), file.get("attributes")]
        .into_iter()
        .flatten()
    {
        for key in [
            "chatgpt_account_id",
            "chatgptAccountId",
            "account_id",
            "accountId",
        ] {
            if let Some(id) = identifier(root.get(key)) {
                return Some(id);
            }
        }
        let token = match root.get("id_token") {
            Some(Value::String(value)) => serde_json::from_str(value).ok(),
            Some(value) => Some(value.clone()),
            None => None,
        };
        if let Some(token) = token {
            for key in ["chatgpt_account_id", "chatgptAccountId"] {
                if let Some(id) = identifier(token.get(key)) {
                    return Some(id);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command, Source, UsageSource, UsageStatus, fetch_snapshot, snapshot_view};
    use clap::Parser;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::Instant,
    };

    struct Request {
        method: String,
        path: String,
        authorization: String,
        body: Value,
    }

    struct Reply {
        status: u16,
        body: String,
        headers: String,
    }

    impl Reply {
        fn json(body: Value) -> Self {
            Self {
                status: 200,
                body: body.to_string(),
                headers: String::new(),
            }
        }

        fn error(status: u16) -> Self {
            Self {
                status,
                body: "sensitive server body: TEST_SECRET".to_owned(),
                headers: String::new(),
            }
        }
    }

    struct Mock {
        url: String,
        worker: thread::JoinHandle<Vec<Request>>,
    }

    impl Mock {
        fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let worker = thread::spawn(move || {
                let mut requests = Vec::new();
                for reply in replies {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let (mut stream, _) = loop {
                        match listener.accept() {
                            Ok(connection) => break connection,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    Instant::now() < deadline,
                                    "mock did not receive expected request"
                                );
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("mock accept: {error}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    requests.push(read_request(&mut stream));
                    write!(stream, "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                        reply.status, reply.body.len(), reply.headers, reply.body).unwrap();
                }
                requests
            });
            Self { url, worker }
        }

        fn session(&self) -> Session {
            Session::new(
                &self.url,
                "TEST_MANAGEMENT_KEY",
                None,
                None,
                None,
                None,
                None,
            )
            .ok()
            .unwrap()
        }

        fn finish(self) -> Vec<Request> {
            self.worker.join().unwrap()
        }
    }

    fn read_request(stream: &mut TcpStream) -> Request {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let header = |name: &str| {
            headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim().to_owned())
        };
        let length: usize = header("content-length").map_or(0, |value| value.parse().unwrap());
        while bytes.len() < header_end + length {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        let first = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>();
        Request {
            method: first[0].to_owned(),
            path: first[1].to_owned(),
            authorization: header("authorization").unwrap_or_default(),
            body: if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
            },
        }
    }

    fn credentials(provider: &str, index: &str) -> Reply {
        Reply::json(
            json!({"files": [{"type": provider, "name": "work-account.json", "auth_index": index,
            "metadata": {"chatgpt_account_id": "work-account"}}]}),
        )
    }

    fn codex_data() -> Value {
        json!({"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 42, "reset_at": 2000, "limit_window_seconds": 18000},
            "secondary_window": {"used_percent": 12, "reset_at": 3000, "limit_window_seconds": 604800}
        }})
    }

    fn claude_data() -> Value {
        json!({"five_hour": {"utilization": 23, "resets_at": "2026-10-10T12:00:00Z"},
            "seven_day": {"utilization": 11, "resets_at": "2026-10-17T12:00:00Z"}})
    }

    fn go_credential(index: &str) -> Value {
        json!({"type": "openai-compatible-opencode-go", "provider": "openai-compatible-opencode-go",
            "label": "opencode-go", "name": "openai-compatibility:opencode-go:runtime",
            "auth_index": index, "runtime_only": true, "disabled": false,
            "account_type": "api_key", "account": "TEST_REMOTE_KEY"})
    }

    fn go_config(index: &str) -> Value {
        json!({"name": "opencode-go", "base-url": GO_BASE_URL, "disabled": false,
            "keys": [{"api-key": "TEST_CONFIG_API_KEY", "auth_index": index,
                "proxy-url": "http://user:TEST_PROXY_SECRET@example.com"}],
            "headers": {"Authorization": "Bearer TEST_HEADER_SECRET"},
            "models": [{"name": "TEST_MODEL_SECRET"}]})
    }

    fn go_data() -> Value {
        json!({"plan": "TEST_SECRET", "usage": {
            "rolling": {"status": "ok", "percent": 12, "resetsAt": "2026-10-10T12:00:00Z"},
            "weekly": {"status": "ok", "percent": 34, "resetsAt": "2026-10-17T12:00:00Z"},
            "monthly": {"status": "ok", "percent": 56, "resetsAt": "2026-11-01T12:00:00Z"}
        }})
    }

    fn envelope(body: Value) -> Reply {
        Reply::json(json!({"status_code": 200, "header": {}, "body": body}))
    }

    #[test]
    fn work_prefixed_doppler_settings_are_supported_without_shell_mapping() {
        let lookup = |name: &str| match name {
            "WORK_LLM_USAGE_CLIPROXY_URL" => Some(" http://localhost:8318 ".to_owned()),
            "WORK_LLM_USAGE_CLIPROXY_MANAGEMENT_KEY" => Some("TEST_KEY".to_owned()),
            "WORK_LLM_USAGE_CLIPROXY_OPENCODE_GO_AUTH_INDEX" => Some("0123456789abcdef".to_owned()),
            "WORK_LLM_USAGE_CLIPROXY_OPENCODE_GO_PROVIDER" => Some("company-go".to_owned()),
            _ => None,
        };
        assert_eq!(
            setting_with(URL_ENV, lookup).as_deref(),
            Some("http://localhost:8318")
        );
        assert_eq!(setting_with(KEY_ENV, lookup).as_deref(), Some("TEST_KEY"));
        assert_eq!(
            setting_with(GO_INDEX_ENV, lookup).as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            setting_with(GO_PROVIDER_ENV, lookup).as_deref(),
            Some("company-go")
        );
    }

    #[test]
    fn canonical_proxy_settings_take_precedence_unless_empty() {
        let lookup = |name: &str| match name {
            "LLM_USAGE_CLIPROXY_URL" => Some("http://canonical:8318".to_owned()),
            "LLM_USAGE_CLIPROXY_MANAGEMENT_KEY" => Some(" ".to_owned()),
            "WORK_LLM_USAGE_CLIPROXY_URL" => Some("http://work:8318".to_owned()),
            "WORK_LLM_USAGE_CLIPROXY_MANAGEMENT_KEY" => Some("TEST_KEY".to_owned()),
            _ => None,
        };
        assert_eq!(
            setting_with(URL_ENV, lookup).as_deref(),
            Some("http://canonical:8318")
        );
        assert_eq!(setting_with(KEY_ENV, lookup).as_deref(), Some("TEST_KEY"));
    }

    #[test]
    fn cli_source_is_explicit_and_local_by_default() {
        let cli = Cli::try_parse_from(["llm-usage", "json"]).unwrap();
        let Some(Command::Json(args)) = cli.command else {
            panic!("expected JSON command")
        };
        assert!(matches!(args.query.source, Source::Local));
        let cli = Cli::try_parse_from([
            "llm-usage",
            "watch",
            "--source",
            "cliproxy",
            "--provider",
            "codex",
        ])
        .unwrap();
        let Some(Command::Watch(args)) = cli.command else {
            panic!("expected watch command")
        };
        assert!(matches!(args.display.query.source, Source::Cliproxy));
        assert_eq!(args.display.query.providers.len(), 1);
        assert!(
            Cli::try_parse_from(["llm-usage", "json", "--management-key", "TEST_SECRET"]).is_err()
        );
    }

    #[test]
    fn remote_codex_snapshot_uses_token_placeholder_and_existing_json_contract() {
        let mock = Mock::start(vec![
            credentials("codex", "codex-1"),
            envelope(codex_data()),
        ]);
        let mut source = UsageSource::Cliproxy(Ok(Box::new(mock.session())));
        let snapshot = fetch_snapshot(&[Provider::Codex], &mut source);
        let view = serde_json::to_value(snapshot_view(&snapshot)).unwrap();
        assert_eq!(view["schema_version"], 2);
        assert_eq!(view["providers"][0]["source"], SOURCE);
        assert_eq!(view["providers"][0]["windows"][0]["used_percent"], 42.0);
        assert_eq!(view["best_available"]["provider"], "codex");
        assert!(!view.to_string().contains("TEST_MANAGEMENT_KEY"));
        let requests = mock.finish();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/v8/management/credentials");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/v8/management/requests/api-call");
        assert_eq!(requests[1].authorization, "Bearer TEST_MANAGEMENT_KEY");
        assert_eq!(requests[1].body["authIndex"], "codex-1");
        assert_eq!(requests[1].body["method"], "GET");
        assert_eq!(requests[1].body["url"], CODEX_USAGE_URL);
        assert_eq!(
            requests[1].body["header"]["Authorization"],
            "Bearer $TOKEN$"
        );
        assert_eq!(
            requests[1].body["header"]["ChatGPT-Account-Id"],
            "work-account"
        );
        assert!(!requests[1].body.to_string().contains("TEST_MANAGEMENT_KEY"));
    }

    #[test]
    fn remote_go_snapshot_uses_server_key_placeholder_and_existing_json_contract() {
        for body in [go_data(), json!(go_data().to_string())] {
            let mock = Mock::start(vec![
                Reply::json(json!({"files": [go_credential("go-1")]})),
                envelope(body),
            ]);
            let mut source = UsageSource::Cliproxy(Ok(Box::new(mock.session())));
            let snapshot = fetch_snapshot(&[Provider::OpencodeGo], &mut source);
            let view = serde_json::to_value(snapshot_view(&snapshot)).unwrap();
            let usage = &view["providers"][0];
            assert_eq!(view["schema_version"], 2);
            assert_eq!(usage["provider"], "opencode-go");
            assert_eq!(usage["source"], SOURCE);
            assert_eq!(usage["available"], true);
            assert_eq!(usage["plan"], "Go");
            assert_eq!(usage["windows"].as_array().unwrap().len(), 3);
            for (i, (label, percent, seconds)) in [
                ("5h", 12.0, 18_000),
                ("7d", 34.0, 604_800),
                ("30d", 56.0, 2_592_000),
            ]
            .into_iter()
            .enumerate()
            {
                assert_eq!(usage["windows"][i]["label"], label);
                assert_eq!(usage["windows"][i]["used_percent"], percent);
                assert_eq!(usage["windows"][i]["window_seconds"], seconds);
                assert!(usage["windows"][i]["reset_at"].as_u64().is_some());
            }
            assert_eq!(view["best_available"]["provider"], "opencode-go");
            let requests = mock.finish();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].path, "/v8/management/credentials");
            assert_eq!(requests[1].method, "POST");
            assert_eq!(requests[1].path, "/v8/management/requests/api-call");
            assert_eq!(requests[1].authorization, "Bearer TEST_MANAGEMENT_KEY");
            assert_eq!(requests[1].body["authIndex"], "go-1");
            assert_eq!(requests[1].body["method"], "GET");
            assert_eq!(requests[1].body["url"], OPENCODE_GO_USAGE_URL);
            assert_eq!(
                requests[1].body["header"]["Authorization"],
                "Bearer $TOKEN$"
            );
            assert!(
                requests[1].body["header"]
                    .get("ChatGPT-Account-Id")
                    .is_none()
            );
            assert!(requests[1].body["header"].get("anthropic-beta").is_none());
            for secret in ["TEST_MANAGEMENT_KEY", "TEST_REMOTE_KEY", "TEST_SECRET"] {
                assert!(!view.to_string().contains(secret));
                assert!(!requests[1].body.to_string().contains(secret));
            }
        }
    }

    #[test]
    fn all_three_remote_providers_share_one_credential_discovery() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": [
                {"type": "codex", "auth_index": "codex-1"},
                go_credential("go-1"),
                {"type": "claude", "auth_index": "claude-1"}
            ]})),
            envelope(codex_data()),
            envelope(go_data()),
            envelope(claude_data()),
        ]);
        let mut source = UsageSource::Cliproxy(Ok(Box::new(mock.session())));
        let snapshot = fetch_snapshot(&[], &mut source);
        assert_eq!(snapshot.providers.len(), 3);
        assert!(
            snapshot
                .providers
                .iter()
                .all(|usage| usage.available && usage.source == Some(SOURCE))
        );
        let requests = mock.finish();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1].body["url"], CODEX_USAGE_URL);
        assert_eq!(requests[2].body["url"], OPENCODE_GO_USAGE_URL);
        assert_eq!(requests[3].body["url"], CLAUDE_CODE_USAGE_URL);
    }

    #[test]
    fn go_selection_requires_exact_group_and_unambiguous_enabled_key() {
        let files = vec![
            go_credential("go-a"),
            go_credential("go-b"),
            json!({"type": "codex", "auth_index": "codex", "label": "opencode-go"}),
            json!({"type": "openai-compatible-openrouter", "auth_index": "other"}),
            json!({"type": "openai-compatibility", "auth_index": "generic"}),
            json!({"type": "openai-compatible-opencode-zen", "auth_index": "zen"}),
            json!({"type": "openai-compatible-opencode-go", "auth_index": "disabled", "disabled": true}),
            json!({"type": "openai-compatible-opencode-go", "auth_index": "status-disabled", "status": "disabled"}),
            json!({"type": "openai-compatible-opencode-go"}),
        ];
        assert!(
            select_credential(&files, Provider::OpencodeGo, None, "opencode-go")
                .err()
                .unwrap()
                .contains(GO_INDEX_ENV)
        );
        assert_eq!(
            select_credential(&files, Provider::OpencodeGo, Some("go-b"), "opencode-go")
                .unwrap()
                .1,
            "go-b"
        );
        for index in [
            "codex",
            "other",
            "generic",
            "zen",
            "disabled",
            "status-disabled",
            "missing",
        ] {
            assert!(
                select_credential(&files, Provider::OpencodeGo, Some(index), "opencode-go")
                    .is_err()
            );
        }
        // Even an explicit group override cannot authorize a known OAuth provider.
        assert!(select_credential(&files, Provider::OpencodeGo, Some("codex"), "codex").is_err());
        for disabled in [json!(true), json!(1), json!(" TRUE "), json!("1")] {
            let mut file = go_credential("go-a");
            file["disabled"] = disabled;
            assert!(
                select_credential(&[file], Provider::OpencodeGo, Some("go-a"), "opencode-go")
                    .is_err()
            );
        }
        for file in [
            go_credential("go-a"),
            json!({"provider": " OPENCODE-GO ", "authIndex": "go-a"}),
            json!({"type": "openai-compatibility", "label": "OpenCode-Go", "auth_index": "go-a"}),
        ] {
            assert_eq!(
                select_credential(&[file], Provider::OpencodeGo, None, "opencode-go")
                    .unwrap()
                    .1,
                "go-a"
            );
        }
    }

    #[test]
    fn go_custom_group_and_index_are_explicitly_selected() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": [
                {"type": "openai-compatible-company-go", "label": "company-go", "auth_index": "a"},
                {"type": "openai-compatible-company-go", "label": "company-go", "auth_index": "b"},
                go_credential("default")
            ]})),
            envelope(go_data()),
        ]);
        let mut session = Session::new(
            &mock.url,
            "TEST_MANAGEMENT_KEY",
            None,
            None,
            Some("b".to_owned()),
            Some("company-go".to_owned()),
            None,
        )
        .ok()
        .unwrap();
        assert!(session.fetch(&[Provider::OpencodeGo], 1000)[0].available);
        let requests = mock.finish();
        assert_eq!(requests[1].body["authIndex"], "b");
        assert_eq!(requests[1].body["url"], OPENCODE_GO_USAGE_URL);
        let file = json!({"provider": "openai-compatible-company-go", "auth_index": "b"});
        assert!(
            select_credential(
                &[file],
                Provider::OpencodeGo,
                None,
                "openai-compatible-company-go"
            )
            .is_ok()
        );
    }

    #[test]
    fn missing_ambiguous_and_unrelated_go_keys_never_trigger_upstream_requests() {
        for files in [
            json!([]),
            json!([go_credential("a"), go_credential("b")]),
            json!([{"type": "openai-compatible-openrouter", "auth_index": "other"}]),
        ] {
            let needs_config = !files
                .as_array()
                .unwrap()
                .iter()
                .any(|file| matches_go_provider(file, "opencode-go"));
            let mut replies = vec![Reply::json(json!({"files": files}))];
            if needs_config {
                replies.push(Reply::json(json!([])));
            }
            let mock = Mock::start(replies);
            let mut session = mock.session();
            let usage = session.fetch(&[Provider::OpencodeGo], 1000);
            assert!(!usage[0].available);
            assert_eq!(usage[0].source, Some(SOURCE));
            assert_eq!(mock.finish().len(), if needs_config { 2 } else { 1 });
        }
    }

    #[test]
    fn explicit_hidden_go_index_uses_existing_server_key_without_reading_config() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": [
                {"type": "codex", "auth_index": "codex-1"},
                {"type": "claude", "auth_index": "claude-1"}
            ]})),
            envelope(codex_data()),
            envelope(go_data()),
            envelope(claude_data()),
        ]);
        let mut session = mock.session();
        session.go_index = Some("0123456789abcdef".to_owned());
        let mut source = UsageSource::Cliproxy(Ok(Box::new(session)));
        let snapshot = fetch_snapshot(&[], &mut source);
        assert!(snapshot.providers.iter().all(|usage| usage.available));
        assert_eq!(snapshot.providers[1].plan.as_deref(), Some("Go"));
        assert_eq!(snapshot.providers[1].source, Some(SOURCE));
        let requests = mock.finish();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[2].body["authIndex"], "0123456789abcdef");
        assert_eq!(requests[2].body["url"], OPENCODE_GO_USAGE_URL);
        assert_eq!(
            requests[2].body["header"]["Authorization"],
            "Bearer $TOKEN$"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.path.contains("config"))
        );
        assert!(
            !serde_json::to_string(&snapshot_view(&snapshot))
                .unwrap()
                .contains("TEST_SECRET")
        );
    }

    #[test]
    fn explicit_go_index_never_bypasses_visible_provider_or_disabled_checks() {
        for file in [
            json!({"type": "codex", "auth_index": "0123456789abcdef"}),
            json!({"type": "claude", "auth_index": "0123456789abcdef"}),
            json!({"type": "openai-compatible-openrouter", "auth_index": "0123456789abcdef"}),
            json!({"type": "openai-compatible-opencode-go", "auth_index": "0123456789abcdef", "disabled": true}),
            json!({"type": "openai-compatible-opencode-go", "auth_index": "0123456789abcdef", "status": "disabled"}),
        ] {
            let mock = Mock::start(vec![Reply::json(json!({"files": [file]}))]);
            let mut session = mock.session();
            session.go_index = Some("0123456789abcdef".to_owned());
            assert!(!session.fetch(&[Provider::OpencodeGo], 1000)[0].available);
            assert_eq!(mock.finish().len(), 1);
        }
    }

    #[test]
    fn malformed_hidden_indexes_are_rejected_without_echo_or_upstream_request() {
        for index in [
            "TEST_SECRET",
            "not-a-runtime-id",
            "0123456789abcdeg",
            "",
            "0123456789abcdef0",
        ] {
            let mock = Mock::start(vec![Reply::json(json!({"files": []}))]);
            let mut session = mock.session();
            session.go_index = Some(index.to_owned());
            let usage = session.fetch(&[Provider::OpencodeGo], 1000);
            assert!(!usage[0].available);
            assert!(
                usage[0]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("hexadecimal auth_index")
            );
            assert!(
                !serde_json::to_string(&usage)
                    .unwrap()
                    .contains("TEST_SECRET")
            );
            assert_eq!(mock.finish().len(), 1);
        }
    }

    #[test]
    fn hidden_go_index_does_not_bypass_failed_discovery_or_echo_rejected_server_body() {
        for response in [Reply::error(401), Reply::error(500)] {
            let mock = Mock::start(vec![response]);
            let mut session = mock.session();
            session.go_index = Some("0123456789abcdef".to_owned());
            assert!(!session.fetch(&[Provider::OpencodeGo], 1000)[0].available);
            assert_eq!(mock.finish().len(), 1);
        }
        let mock = Mock::start(vec![Reply::json(json!({"files": []})), Reply::error(400)]);
        let mut session = mock.session();
        session.go_index = Some("0123456789abcdef".to_owned());
        let usage = session.fetch(&[Provider::OpencodeGo], 1000);
        assert!(!usage[0].available);
        assert!(
            usage[0]
                .error
                .as_deref()
                .unwrap()
                .contains("configured Go auth_index")
        );
        assert!(
            !serde_json::to_string(&usage)
                .unwrap()
                .contains("TEST_SECRET")
        );
        assert_eq!(mock.finish().len(), 2);
    }

    #[test]
    fn automatic_go_discovery_reads_scoped_config_and_retains_only_safe_metadata() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": []})),
            Reply::json(json!([go_config("0123456789abcdef")])),
            envelope(go_data()),
        ]);
        let mut session = mock.session();
        let usage = session.fetch(&[Provider::OpencodeGo], 1000);
        assert!(usage[0].available);
        assert_eq!(usage[0].plan.as_deref(), Some("Go"));
        assert_eq!(usage[0].windows.len(), 3);
        let metadata = session
            .go_discovery
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .ok()
            .unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0]["auth_index"], "0123456789abcdef");
        assert_eq!(metadata[0]["base_url"], GO_BASE_URL);
        assert!(metadata[0].get("keys").is_none());
        assert!(metadata[0].get("api-key").is_none());
        let cached = serde_json::to_string(metadata).unwrap();
        let output = serde_json::to_string(&usage).unwrap();
        let requests = mock.finish();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].method, "GET");
        assert_eq!(
            requests[1].path,
            "/v8/management/config/api-keys/openai-compatibility"
        );
        assert_eq!(requests[1].authorization, "Bearer TEST_MANAGEMENT_KEY");
        assert_eq!(requests[2].body["authIndex"], "0123456789abcdef");
        assert_eq!(requests[2].body["url"], OPENCODE_GO_USAGE_URL);
        assert_eq!(
            requests[2].body["header"]["Authorization"],
            "Bearer $TOKEN$"
        );
        for secret in [
            "TEST_CONFIG_API_KEY",
            "TEST_PROXY_SECRET",
            "TEST_HEADER_SECRET",
            "TEST_MODEL_SECRET",
        ] {
            assert!(!cached.contains(secret));
            assert!(!output.contains(secret));
            assert!(
                requests
                    .iter()
                    .all(|request| !request.body.to_string().contains(secret))
            );
        }
    }

    #[test]
    fn automatic_hidden_go_discovery_keeps_other_providers_working() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": [
                {"type": "codex", "auth_index": "codex-1"},
                {"type": "claude", "auth_index": "claude-1"}
            ]})),
            envelope(codex_data()),
            Reply::json(json!([go_config("0123456789abcdef")])),
            envelope(go_data()),
            envelope(claude_data()),
        ]);
        let mut source = UsageSource::Cliproxy(Ok(Box::new(mock.session())));
        let snapshot = fetch_snapshot(&[], &mut source);
        assert_eq!(snapshot.providers.len(), 3);
        assert!(
            snapshot
                .providers
                .iter()
                .all(|usage| usage.available && usage.source == Some(SOURCE))
        );
        assert_eq!(mock.finish().len(), 5);
    }

    #[test]
    fn automatic_go_metadata_is_cached_without_keys_and_refreshes_for_rotation() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": []})),
            Reply::json(json!([go_config("0123456789abcdef")])),
            envelope(go_data()),
            Reply::json(json!({"files": []})),
            envelope(go_data()),
            Reply::json(json!({"files": []})),
            Reply::json(json!([go_config("fedcba9876543210")])),
            envelope(go_data()),
        ]);
        let mut session = mock.session();
        for fetched_at in [1000, 1030, 1300] {
            assert!(session.fetch(&[Provider::OpencodeGo], fetched_at)[0].available);
        }
        let requests = mock.finish();
        assert_eq!(requests.len(), 8);
        assert_eq!(requests[2].body["authIndex"], "0123456789abcdef");
        assert_eq!(requests[4].body["authIndex"], "0123456789abcdef");
        assert_eq!(requests[7].body["authIndex"], "fedcba9876543210");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path.contains("config/"))
                .count(),
            2
        );
    }

    #[test]
    fn scoped_go_config_requires_exact_enabled_group_safe_url_and_valid_indexes() {
        let other = json!({"name": "openrouter", "base-url": "https://openrouter.ai/api/v1",
            "keys": [{"api-key": "TEST_SECRET", "auth_index": "1111111111111111"}]});
        let mut disabled = go_config("2222222222222222");
        disabled["disabled"] = json!(true);
        let groups: Vec<GoConfigGroup> =
            serde_json::from_value(json!([other, disabled, go_config("0123456789abcdef")]))
                .unwrap();
        let metadata = sanitized_go_credentials(groups, "opencode-go")
            .ok()
            .unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0]["auth_index"], "0123456789abcdef");
        for url in [
            "http://opencode.ai/zen/go/v1",
            "https://opencode.ai.evil.test/zen/go/v1",
            "https://opencode.ai/zen/v1",
            "https://opencode.ai:8443/zen/go/v1",
            "https://user:TEST_SECRET@opencode.ai/zen/go/v1",
            "https://opencode.ai/zen/go/v1?key=TEST_SECRET",
            "https://opencode.ai/zen/go/v1#TEST_SECRET",
        ] {
            let mut group = go_config("0123456789abcdef");
            group["base-url"] = json!(url);
            let groups = serde_json::from_value(json!([group])).unwrap();
            let failure = sanitized_go_credentials(groups, "opencode-go")
                .err()
                .unwrap();
            assert!(!failure.message.contains("TEST_SECRET"));
        }
        for index in [
            json!(null),
            json!(""),
            json!("TEST_SECRET"),
            json!("0123456789abcdeg"),
        ] {
            let mut group = go_config("0123456789abcdef");
            group["keys"][0]["auth_index"] = index;
            let groups = serde_json::from_value(json!([group])).unwrap();
            assert!(sanitized_go_credentials(groups, "opencode-go").is_err());
        }
        let mut disabled_key = go_config("0123456789abcdef");
        disabled_key["keys"][0]["disabled"] = json!(true);
        let groups = serde_json::from_value(json!([disabled_key])).unwrap();
        assert!(
            sanitized_go_credentials(groups, "opencode-go")
                .ok()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn automatic_go_config_supports_custom_group_and_rejects_ambiguity() {
        let mut custom = go_config("0123456789abcdef");
        custom["name"] = json!("Company-Go");
        custom["base-url"] = json!(format!("{GO_BASE_URL}/"));
        let mock = Mock::start(vec![
            Reply::json(json!({"files": []})),
            Reply::json(json!([custom])),
            envelope(go_data()),
        ]);
        let mut session = mock.session();
        session.go_provider = "openai-compatible-company-go".to_owned();
        assert!(session.fetch(&[Provider::OpencodeGo], 1000)[0].available);
        assert_eq!(mock.finish().len(), 3);
        let mock = Mock::start(vec![
            Reply::json(json!({"files": []})),
            Reply::json(json!([
                go_config("0123456789abcdef"),
                go_config("fedcba9876543210")
            ])),
        ]);
        let usage = mock.session().fetch(&[Provider::OpencodeGo], 1000);
        assert!(!usage[0].available);
        assert!(usage[0].error.as_deref().unwrap().contains(GO_INDEX_ENV));
        assert_eq!(mock.finish().len(), 2);
    }

    #[test]
    fn config_discovery_never_overrides_visible_unrelated_or_disabled_indexes() {
        for file in [
            json!({"type": "codex", "auth_index": "0123456789abcdef"}),
            json!({"type": "openai-compatible-openrouter", "auth_index": "0123456789abcdef", "disabled": true}),
        ] {
            let mock = Mock::start(vec![
                Reply::json(json!({"files": [file]})),
                Reply::json(json!([go_config("0123456789abcdef")])),
            ]);
            let usage = mock.session().fetch(&[Provider::OpencodeGo], 1000);
            assert!(!usage[0].available);
            assert_eq!(mock.finish().len(), 2);
        }
        let mock = Mock::start(vec![Reply::json(json!({"files": [
            {"type": "openai-compatible-opencode-go", "auth_index": "0123456789abcdef", "disabled": true}
        ]}))]);
        assert!(!mock.session().fetch(&[Provider::OpencodeGo], 1000)[0].available);
        assert_eq!(mock.finish().len(), 1);
    }

    #[test]
    fn go_config_failures_are_redacted_cached_and_do_not_break_codex() {
        for response in [
            Reply::error(401),
            Reply::error(403),
            Reply::error(404),
            Reply::error(500),
            Reply::json(json!({"raw": "TEST_SECRET"})),
            Reply::json(
                json!([{"name": "opencode-go", "base-url": GO_BASE_URL, "keys": "TEST_SECRET"}]),
            ),
        ] {
            let mock = Mock::start(vec![
                credentials("codex", "codex-1"),
                envelope(codex_data()),
                response,
                credentials("codex", "codex-1"),
                envelope(codex_data()),
            ]);
            let mut session = mock.session();
            for fetched_at in [1000, 1030] {
                let usage = session.fetch(&[Provider::Codex, Provider::OpencodeGo], fetched_at);
                assert!(usage[0].available);
                assert!(!usage[1].available);
                assert!(
                    !serde_json::to_string(&usage)
                        .unwrap()
                        .contains("TEST_SECRET")
                );
            }
            assert_eq!(mock.finish().len(), 5);
        }
    }

    #[test]
    fn duplicate_config_indexes_fail_closed_and_no_go_selection_avoids_config_reads() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": []})),
            Reply::json(json!([
                go_config("0123456789abcdef"),
                go_config("0123456789abcdef")
            ])),
        ]);
        let usage = mock.session().fetch(&[Provider::OpencodeGo], 1000);
        assert!(!usage[0].available);
        assert!(
            usage[0]
                .error
                .as_deref()
                .unwrap()
                .contains("duplicate auth_index")
        );
        assert_eq!(mock.finish().len(), 2);
        let mock = Mock::start(vec![
            credentials("codex", "codex-1"),
            envelope(codex_data()),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::Codex], 1000)[0].available);
        assert!(session.go_discovery.is_none());
        assert_eq!(mock.finish().len(), 2);
    }

    #[test]
    fn visible_go_index_collisions_fail_closed_even_with_explicit_selection() {
        for foreign in [
            json!({"type": "codex", "auth_index": "0123456789abcdef"}),
            json!({"type": "openai-compatible-opencode-go", "auth_index": "0123456789abcdef", "disabled": true}),
        ] {
            for selector in [None, Some("0123456789abcdef".to_owned())] {
                let mock = Mock::start(vec![Reply::json(json!({"files": [
                    go_credential("0123456789abcdef"), foreign
                ]}))]);
                let mut session = mock.session();
                session.go_index = selector;
                let usage = session.fetch(&[Provider::OpencodeGo], 1000);
                assert!(!usage[0].available);
                assert!(
                    usage[0]
                        .error
                        .as_deref()
                        .unwrap()
                        .contains("shares an auth_index")
                );
                assert_eq!(mock.finish().len(), 1);
            }
        }
    }

    #[test]
    fn config_index_collision_does_not_silently_make_ambiguous_keys_unambiguous() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": [{"type": "codex", "auth_index": "0123456789abcdef"}]})),
            Reply::json(json!([
                go_config("0123456789abcdef"),
                go_config("fedcba9876543210")
            ])),
        ]);
        let usage = mock.session().fetch(&[Provider::OpencodeGo], 1000);
        assert!(!usage[0].available);
        assert!(
            usage[0]
                .error
                .as_deref()
                .unwrap()
                .contains("conflicts with visible")
        );
        assert_eq!(mock.finish().len(), 2);
    }

    #[test]
    fn go_upstream_errors_and_invalid_usage_are_unavailable_and_redacted() {
        for response in [
            Reply::error(400),
            Reply::json(json!({"status_code": 401, "body": "TEST_SECRET"})),
            Reply::json(json!({"status_code": 403, "body": "TEST_SECRET"})),
            Reply::json(
                json!({"status_code": 429, "header": {"Retry-After": "60"}, "body": "TEST_SECRET"}),
            ),
            Reply::json(json!({"status_code": 500, "body": "TEST_SECRET"})),
            envelope(json!({"usage": "TEST_SECRET"})),
            envelope(json!({"usage": {"rolling": {"percent": 10, "resetsAt": "TEST_SECRET"}}})),
        ] {
            let mock = Mock::start(vec![
                Reply::json(json!({"files": [go_credential("go-1")]})),
                response,
            ]);
            let usage = mock.session().fetch(&[Provider::OpencodeGo], 1000);
            assert!(!usage[0].available);
            assert_eq!(usage[0].source, Some(SOURCE));
            let output = serde_json::to_string(&usage).unwrap();
            for secret in ["TEST_SECRET", "TEST_REMOTE_KEY", "TEST_MANAGEMENT_KEY"] {
                assert!(!output.contains(secret));
            }
            assert_eq!(mock.finish().len(), 2);
        }
    }

    #[test]
    fn go_rate_limited_windows_reuse_existing_provider_status() {
        let mut data = go_data();
        data["usage"]["rolling"]["status"] = json!("rate-limited");
        let usage = usage_from_data(Provider::OpencodeGo, &data, 1000)
            .ok()
            .unwrap();
        assert!(usage.available);
        assert_eq!(usage.status, UsageStatus::RateLimited);
        assert_eq!(usage.windows[0].limit_reached, Some(true));
        assert_eq!(usage.windows[0].used_percent, Some(12.0));
    }

    #[test]
    fn claude_string_body_throttles_requests_without_marking_success_stale() {
        let mock = Mock::start(vec![
            credentials("claude", "claude-1"),
            envelope(json!(claude_data().to_string())),
            credentials("claude", "claude-1"),
        ]);
        let mut session = mock.session();
        let first = session.fetch(&[Provider::ClaudeCode], 1000);
        let second = session.fetch(&[Provider::ClaudeCode], 1030);
        assert_eq!(first[0].windows[0].used_percent, Some(23.0));
        assert_eq!(second[0].status, UsageStatus::Ok);
        assert_eq!(second[0].fetched_at, 1000);
        assert!(second[0].error.is_none());
        assert_eq!(second[0].source, Some(SOURCE));
        let requests = mock.finish();
        assert_eq!(requests.len(), 3); // two discoveries, only one upstream request
        assert_eq!(requests[1].body["url"], CLAUDE_CODE_USAGE_URL);
        assert_eq!(
            requests[1].body["header"]["anthropic-beta"],
            "oauth-2025-04-20"
        );
        assert_eq!(
            requests[1].body["header"]["Authorization"],
            "Bearer $TOKEN$"
        );
    }

    #[test]
    fn credential_selection_requires_unambiguous_enabled_matching_account() {
        let files = vec![
            json!({"type": "codex", "auth_index": "disabled", "disabled": true}),
            json!({"type": "codex", "auth_index": "a"}),
            json!({"provider": "codex", "authIndex": 2}),
            json!({"type": "claude", "auth_index": "c"}),
            json!({"type": "codex"}),
        ];
        assert!(
            select_credential(&files, Provider::Codex, None, "opencode-go")
                .err()
                .unwrap()
                .contains(CODEX_INDEX_ENV)
        );
        assert_eq!(
            select_credential(&files, Provider::Codex, Some("2"), "opencode-go")
                .unwrap()
                .1,
            "2"
        );
        assert!(
            select_credential(&files, Provider::Codex, Some("disabled"), "opencode-go").is_err()
        );
        assert!(select_credential(&files, Provider::Codex, Some("c"), "opencode-go").is_err());
        assert!(select_credential(&[], Provider::Codex, None, "opencode-go").is_err());
        assert_eq!(
            select_credential(&files, Provider::ClaudeCode, None, "opencode-go")
                .unwrap()
                .1,
            "c"
        );
    }

    #[test]
    fn disabled_metadata_variants_are_never_selected() {
        for disabled in [json!(true), json!(1), json!(" TRUE "), json!("1")] {
            let files = vec![json!({"type": "codex", "auth_index": "a", "disabled": disabled})];
            assert!(select_credential(&files, Provider::Codex, Some("a"), "opencode-go").is_err());
        }
    }

    #[test]
    fn ambiguous_accounts_never_trigger_upstream_requests() {
        let mock = Mock::start(vec![Reply::json(json!({"files": [
            {"type": "codex", "auth_index": "a"}, {"type": "codex", "auth_index": "b"}]}))]);
        let usage = mock.session().fetch(&[Provider::Codex], 1000);
        assert!(!usage[0].available);
        assert!(usage[0].error.as_ref().unwrap().contains(CODEX_INDEX_ENV));
        assert_eq!(mock.finish().len(), 1);
    }

    #[test]
    fn explicit_selector_and_account_override_are_sent() {
        let mock = Mock::start(vec![
            Reply::json(json!({"files": [
            {"type": "codex", "auth_index": "a"}, {"type": "codex", "auth_index": "b"}]})),
            envelope(codex_data()),
        ]);
        let mut session = Session::new(
            &format!("{}/v8/management/", mock.url),
            "TEST_MANAGEMENT_KEY",
            Some("b".to_owned()),
            None,
            None,
            None,
            Some("override-account".to_owned()),
        )
        .ok()
        .unwrap();
        assert!(session.fetch(&[Provider::Codex], 1000)[0].available);
        let requests = mock.finish();
        assert_eq!(requests[1].body["authIndex"], "b");
        assert_eq!(
            requests[1].body["header"]["ChatGPT-Account-Id"],
            "override-account"
        );
    }

    #[test]
    fn account_ids_are_extracted_from_metadata_attributes_and_json_id_tokens() {
        for file in [
            json!({"chatgpt_account_id": "account"}),
            json!({"metadata": {"chatgptAccountId": "account"}}),
            json!({"attributes": {"id_token": {"chatgpt_account_id": "account"}}}),
            json!({"id_token": "{\"chatgpt_account_id\":\"account\"}"}),
        ] {
            assert_eq!(account_id(&file).as_deref(), Some("account"));
        }
        assert!(account_id(&json!({"id_token": "invalid JWT"})).is_none());
    }

    #[test]
    fn remote_configuration_rejects_unsafe_urls_and_keys_without_echoing_them() {
        for url in [
            "not a URL TEST_SECRET",
            "file:///tmp/TEST_SECRET",
            "http://user:TEST_SECRET@example.com",
            "http://example.com?key=TEST_SECRET",
            "http://example.com/#TEST_SECRET",
            "http://example.com/management.html",
        ] {
            let message = Session::new(url, "TEST_KEY", None, None, None, None, None)
                .err()
                .unwrap();
            assert!(!message.contains("TEST_SECRET"));
        }
        assert!(
            Session::new(
                "http://localhost:8318",
                "TEST_SECRET\n",
                None,
                None,
                None,
                None,
                None
            )
            .is_err()
        );
        assert!(Session::new("http://localhost:8318", "", None, None, None, None, None).is_err());
        for url in [
            "http://localhost:8318",
            "https://localhost:8318/v8/management",
        ] {
            let session = Session::new(url, "TEST_KEY", None, None, None, None, None)
                .ok()
                .unwrap();
            assert_eq!(session.base.path(), "/v8/management/");
            assert!(session.authorization.is_sensitive());
        }
    }

    #[test]
    fn management_and_upstream_authentication_failures_are_distinct_and_redacted() {
        let mock = Mock::start(vec![Reply::error(401)]);
        let usage = mock.session().fetch(&[Provider::Codex], 1000);
        assert_eq!(
            usage[0].error.as_deref(),
            Some("CLIProxyAPI management key was rejected")
        );
        assert_eq!(mock.finish().len(), 1);
        let mock = Mock::start(vec![
            credentials("codex", "a"),
            Reply::json(json!({"status_code": 403, "body": "TEST_SECRET"})),
        ]);
        let usage = mock.session().fetch(&[Provider::Codex], 1000);
        assert_eq!(
            usage[0].error.as_deref(),
            Some("CLIProxyAPI remote provider credential was rejected")
        );
        assert!(
            !serde_json::to_string(&usage)
                .unwrap()
                .contains("TEST_SECRET")
        );
        assert_eq!(mock.finish().len(), 2);
    }

    #[test]
    fn malformed_envelopes_and_windows_are_unavailable_not_placeholder_success() {
        for envelope in [
            json!({}),
            json!({"status_code": "200", "body": {}}),
            json!({"status_code": 200, "body": "TEST_SECRET"}),
            json!({"status_code": 200, "body": []}),
            json!({"status_code": 999999, "body": {}}),
        ] {
            let failure = parse_envelope(&envelope).err().unwrap();
            assert_eq!(failure.message, INVALID_RESPONSE);
        }
        for provider in [Provider::Codex, Provider::ClaudeCode, Provider::OpencodeGo] {
            assert!(usage_from_data(provider, &json!({}), 1000).is_err());
        }
        let mock = Mock::start(vec![Reply::json(json!({"files": "TEST_SECRET"}))]);
        let usage = mock.session().fetch(&[Provider::Codex], 1000);
        assert_eq!(usage[0].error.as_deref(), Some(INVALID_RESPONSE));
        mock.finish();
    }

    #[test]
    fn rate_limits_obey_retry_after_array_case_and_default_without_body_disclosure() {
        for (header, expected) in [
            (json!({"Retry-After": ["1200"]}), 1200),
            (json!({"retry-after": "30"}), 60),
            (json!({}), 900),
            (json!({"retry-after": "invalid TEST_SECRET"}), 900),
        ] {
            let failure = parse_envelope(
                &json!({"status_code": 429, "header": header, "body": "TEST_SECRET"}),
            )
            .err()
            .unwrap();
            assert_eq!(failure.delay, expected);
            assert_eq!(failure.cooldown, ClaudeCodeCooldown::RateLimited);
            assert!(!failure.message.contains("TEST_SECRET"));
        }
    }

    #[test]
    fn claude_rate_limit_retains_stale_usage_and_suppresses_calls_until_retry() {
        let mock = Mock::start(vec![
            credentials("claude", "c"),
            envelope(claude_data()),
            credentials("claude", "c"),
            Reply::json(
                json!({"status_code": 429, "header": {"Retry-After": ["1200"]}, "body": "TEST_SECRET"}),
            ),
            credentials("claude", "c"),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::ClaudeCode], 1000)[0].available);
        let stale = session.fetch(&[Provider::ClaudeCode], 1300);
        assert_eq!(stale[0].status, UsageStatus::Stale);
        assert_eq!(stale[0].source, Some(SOURCE));
        assert_eq!(stale[0].fetched_at, 1000);
        assert!(stale[0].error.as_ref().unwrap().contains("20m"));
        assert_eq!(
            session.fetch(&[Provider::ClaudeCode], 1400)[0].status,
            UsageStatus::Stale
        );
        assert_eq!(mock.finish().len(), 5);
    }

    #[test]
    fn claude_transient_failure_is_stale_but_auth_failure_clears_cached_usage() {
        let mock = Mock::start(vec![
            credentials("claude", "c"),
            envelope(claude_data()),
            credentials("claude", "c"),
            Reply::json(json!({"status_code": 500, "body": "TEST_SECRET"})),
            credentials("claude", "c"),
            Reply::json(json!({"status_code": 401, "body": "TEST_SECRET"})),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::ClaudeCode], 1000)[0].available);
        assert_eq!(
            session.fetch(&[Provider::ClaudeCode], 1300)[0].status,
            UsageStatus::Stale
        );
        let rejected = session.fetch(&[Provider::ClaudeCode], 1600);
        assert!(!rejected[0].available);
        assert!(session.claude_cache.is_empty());
        assert!(
            !serde_json::to_string(&rejected)
                .unwrap()
                .contains("TEST_SECRET")
        );
        assert_eq!(mock.finish().len(), 6);
    }

    #[test]
    fn management_auth_failure_discards_remote_cache_and_stale_data_expires() {
        let mock = Mock::start(vec![
            credentials("claude", "c"),
            envelope(claude_data()),
            Reply::error(401),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::ClaudeCode], 1000)[0].available);
        assert!(!session.fetch(&[Provider::ClaudeCode], 1030)[0].available);
        assert!(session.claude_cache.is_empty());
        mock.finish();
        let cache = ClaudeCodeCache::from_usage(
            &usage_from_data(Provider::ClaudeCode, &claude_data(), 1000)
                .ok()
                .unwrap(),
            6000,
        );
        assert!(remote_cached_usage(&cache, 4601, "retry").is_none());
    }

    #[test]
    fn remote_cache_is_isolated_between_accounts_and_server_sessions() {
        let mock = Mock::start(vec![
            credentials("claude", "a"),
            envelope(claude_data()),
            credentials("claude", "b"),
            Reply::json(json!({"status_code": 500})),
            Reply::json(
                json!({"files": [{"type": "claude", "auth_index": "a", "name": "different-account.json"}]}),
            ),
            Reply::json(json!({"status_code": 500})),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::ClaudeCode], 1000)[0].available);
        assert!(!session.fetch(&[Provider::ClaudeCode], 1030)[0].available);
        assert!(!session.fetch(&[Provider::ClaudeCode], 1060)[0].available);
        assert!(mock.session().claude_cache.is_empty());
        assert_eq!(mock.finish().len(), 6);
    }

    #[test]
    fn proxy_errors_and_missing_go_never_fall_back_to_local_credentials() {
        // Exercise the same dispatch as watch/once/json. No local transport is constructed.
        let mut source = UsageSource::Cliproxy(Err("CLIProxyAPI configuration missing"));
        let snapshot = fetch_snapshot(&[], &mut source);
        assert_eq!(snapshot.providers.len(), 3);
        assert!(
            snapshot
                .providers
                .iter()
                .all(|usage| !usage.available && usage.source == Some(SOURCE))
        );
        let mock = Mock::start(vec![
            Reply::json(json!({"files": []})),
            Reply::json(json!([])),
        ]);
        let mut source = UsageSource::Cliproxy(Ok(Box::new(mock.session())));
        let snapshot = fetch_snapshot(&[Provider::OpencodeGo], &mut source);
        assert!(!snapshot.providers[0].available);
        assert_eq!(snapshot.providers[0].source, Some(SOURCE));
        assert!(
            snapshot.providers[0]
                .error
                .as_ref()
                .unwrap()
                .contains("No matching enabled")
        );
        assert_eq!(mock.finish().len(), 2);
    }

    #[test]
    fn redirects_do_not_receive_management_key() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let mock = Mock::start(vec![Reply {
            status: 302,
            body: String::new(),
            headers: format!(
                "Location: http://{}/stolen\r\n",
                target.local_addr().unwrap()
            ),
        }]);
        let usage = mock.session().fetch(&[Provider::Codex], 1000);
        assert!(!usage[0].available);
        assert_eq!(
            target.accept().err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(mock.finish().len(), 1);
    }

    #[test]
    fn same_index_and_filename_with_changed_claude_identity_never_reuses_usage() {
        let file = |email: &str| {
            Reply::json(json!({"files": [{"type": "claude", "auth_index": "c",
            "name": "work.json", "metadata": {"email": email}}]}))
        };
        let mock = Mock::start(vec![
            file("first@example.test"),
            envelope(claude_data()),
            file("second@example.test"),
            Reply::json(json!({"status_code": 500})),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::ClaudeCode], 1000)[0].available);
        assert!(!session.fetch(&[Provider::ClaudeCode], 1030)[0].available);
        assert_eq!(mock.finish().len(), 4);
    }

    #[test]
    fn discovery_failure_retains_bounded_stale_usage_and_backs_off() {
        let mock = Mock::start(vec![
            credentials("claude", "c"),
            envelope(claude_data()),
            Reply::error(500),
        ]);
        let mut session = mock.session();
        assert!(session.fetch(&[Provider::ClaudeCode], 1000)[0].available);
        let stale = session.fetch(&[Provider::ClaudeCode], 1030);
        assert_eq!(stale[0].status, UsageStatus::Stale);
        assert_eq!(stale[0].fetched_at, 1000);
        assert_eq!(
            session.fetch(&[Provider::ClaudeCode], 1060)[0].status,
            UsageStatus::Stale
        );
        assert_eq!(mock.finish().len(), 3);
    }

    #[test]
    fn management_rate_limit_honors_retry_after_without_additional_discovery() {
        let mock = Mock::start(vec![Reply {
            status: 429,
            body: "TEST_SECRET".to_owned(),
            headers: "Retry-After: 1200\r\n".to_owned(),
        }]);
        let mut session = mock.session();
        assert!(!session.fetch(&[Provider::Codex], 1000)[0].available);
        assert!(!session.fetch(&[Provider::Codex], 1030)[0].available);
        assert_eq!(session.discovery_failure.as_ref().unwrap().0, 2200);
        assert_eq!(mock.finish().len(), 1);
    }

    #[test]
    fn credential_revision_changes_invalidate_cache_but_secret_fields_do_not_enter_key() {
        let first = json!({"name": "work.json", "email": "work@example.test", "modified_at": "before", "access_token": "TEST_SECRET"});
        let mut second = first.clone();
        second["modified_at"] = json!("after");
        assert_ne!(
            credential_identity(&first, "c"),
            credential_identity(&second, "c")
        );
        assert!(!credential_identity(&first, "c").contains("TEST_SECRET"));
    }

    #[test]
    fn unsafe_plan_text_is_not_emitted_or_cached() {
        let mut data = codex_data();
        data["plan_type"] = json!("TEST_SECRET");
        let usage = usage_from_data(Provider::Codex, &data, 1000).ok().unwrap();
        assert!(usage.plan.is_none());
        assert!(
            !serde_json::to_string(&usage)
                .unwrap()
                .contains("TEST_SECRET")
        );
    }
}
