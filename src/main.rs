use std::{
    env,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Local, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size},
};
use reqwest::{
    StatusCode,
    blocking::Client,
    header::{HeaderValue, RETRY_AFTER},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const JSON_SCHEMA_VERSION: u8 = 2;
const BAR_WIDTH: usize = 28;
const BAR_WIDTH_COMPACT: usize = 16;
const PROVIDER_WIDTH: usize = 11;
const WINDOW_WIDTH: usize = 6;
const COMPACT_WINDOW_WIDTH: usize = 3;
const RESET_WIDTH: usize = "unavailable".len();
/// Single-column mark for stale presentation status, kept narrow so the
/// fixed-width provider column stays aligned.
const STALE_MARK: &str = "≈";
const WARNING_PERCENT: u8 = 70;
const CRITICAL_PERCENT: u8 = 90;
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CLAUDE_CODE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_CODE_REQUEST_ERROR: &str = "Claude Code usage request failed";
const CLAUDE_CODE_CACHE_SCHEMA_VERSION: u8 = 1;
const CLAUDE_CODE_CACHE_MAX_AGE: u64 = 60 * 60;
const CLAUDE_CODE_REFRESH_INTERVAL: u64 = 5 * 60;
const CLAUDE_CODE_RATE_LIMIT_COOLDOWN: u64 = 15 * 60;
const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

#[derive(Parser)]
#[command(about = "Show subscription quotas for Codex, OpenCode Go, and Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Refresh the terminal dashboard until interrupted.
    Watch(WatchArgs),
    /// Print one terminal dashboard snapshot.
    Once(DisplayArgs),
    /// Print one JSON snapshot.
    Json(QueryArgs),
}

#[derive(Args, Default)]
struct QueryArgs {
    /// Restrict output to one or more providers.
    #[arg(long = "provider", value_enum)]
    providers: Vec<Provider>,
}

#[derive(Args, Default)]
struct DisplayArgs {
    #[command(flatten)]
    query: QueryArgs,
    /// Disable ANSI colors.
    #[arg(long)]
    no_color: bool,
    /// Force compact layout for narrow terminals.
    #[arg(long)]
    compact: bool,
}

#[derive(Args)]
struct WatchArgs {
    #[command(flatten)]
    display: DisplayArgs,
    /// Seconds between refreshes.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    interval: u64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum Provider {
    Codex,
    OpencodeGo,
    ClaudeCode,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpencodeGo => "opencode-go",
            Self::ClaudeCode => "claude-code",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::OpencodeGo => "OpenCode Go",
            Self::ClaudeCode => "Claude Code",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatus {
    Ok,
    Stale,
    Unavailable,
    RateLimited,
}

#[derive(Clone, Serialize)]
struct Snapshot {
    schema_version: u8,
    fetched_at: u64,
    providers: Vec<ProviderUsage>,
}

#[derive(Clone, Serialize)]
struct ProviderUsage {
    provider: &'static str,
    status: UsageStatus,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    windows: Vec<UsageWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    fetched_at: u64,
}

#[derive(Clone, Serialize)]
struct UsageWindow {
    label: &'static str,
    status: UsageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_reached: Option<bool>,
}

/// Serialized view of a [`Snapshot`] with derived quota state added.
///
/// Every raw provider field is preserved; `display` and `best_available` are
/// additive conveniences so consumers do not reimplement quota semantics.
#[derive(Serialize)]
struct SnapshotView<'a> {
    schema_version: u8,
    fetched_at: u64,
    providers: Vec<ProviderView<'a>>,
    best_available: Option<BestAvailable>,
    presentation: Presentation,
}

#[derive(Serialize)]
struct ProviderView<'a> {
    #[serde(flatten)]
    usage: &'a ProviderUsage,
    display: ProviderDisplay,
}

#[derive(Serialize)]
struct ProviderDisplay {
    name: &'static str,
    exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capacity_used_percent: Option<u8>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    windows: Vec<DisplayWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limiting_window: Option<DisplayWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_reset_at: Option<u64>,
}

#[derive(Serialize)]
struct DisplayWindow {
    label: &'static str,
    used_percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_at: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
struct BestAvailable {
    provider: &'static str,
    capacity_used_percent: u8,
}

/// Compact human-facing status text for any widget or dashboard. It carries no
/// colors, no markup, and no shell; consumers style it themselves.
#[derive(Serialize)]
struct Presentation {
    summary: String,
    severity: Severity,
    providers: Vec<ProviderPresentation>,
    freshness: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Severity {
    Ok,
    Warning,
    Critical,
    Unknown,
}

#[derive(Serialize)]
struct ProviderPresentation {
    provider: &'static str,
    label: String,
    visible: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClaudeCodeCooldown {
    RateLimited,
    Transient,
}

enum ClaudeCodeRequestError {
    Authentication,
    Retry {
        delay: u64,
        cooldown: ClaudeCodeCooldown,
        message: String,
    },
}

#[derive(Clone, Deserialize, Serialize)]
struct ClaudeCodeCache {
    schema_version: u8,
    fetched_at: u64,
    refresh_after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cooldown: Option<ClaudeCodeCooldown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    windows: Vec<CachedUsageWindow>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CachedUsageWindow {
    label: String,
    status: UsageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_reached: Option<bool>,
}

fn main() {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Watch(WatchArgs {
        display: DisplayArgs::default(),
        interval: 30,
    }));

    let exit = match command {
        Command::Watch(args) => watch(&args),
        Command::Once(args) => print_once(&args),
        Command::Json(args) => print_json(&args),
    };
    std::process::exit(exit);
}

fn watch(args: &WatchArgs) -> i32 {
    let terminal = io::stdout().is_terminal();
    let colors = terminal && !args.display.no_color && env::var_os("NO_COLOR").is_none();
    loop {
        let dimensions = terminal.then(size).and_then(Result::ok);
        let columns = dimensions.map(|(columns, _)| columns);
        let snapshot = fetch_snapshot(&args.display.query.providers);
        let layout = dashboard_layout(&snapshot, args.display.compact, columns);
        let hint = refresh_hint(args.interval, args.interval, columns);
        let dashboard = render_dashboard_with_hint(&snapshot, colors, layout, Some(&hint), columns);
        if terminal {
            print!("\x1b[2J\x1b[H");
        }
        println!("{dashboard}");
        let _ = io::stdout().flush();
        let raw_mode = io::stdin()
            .is_terminal()
            .then(RawMode::enable)
            .transpose()
            .ok()
            .flatten();
        let deadline = Instant::now() + Duration::from_secs(args.interval);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = if terminal {
                remaining.min(Duration::from_secs(1))
            } else {
                remaining
            };
            match wait_for_action(timeout, raw_mode.is_some()) {
                WaitAction::Exit => return 0,
                WaitAction::Resize => break,
                WaitAction::Timeout => {
                    let remaining = seconds_remaining(deadline);
                    if terminal {
                        update_dashboard_title(
                            colors,
                            &refresh_hint(args.interval, remaining, columns),
                            columns,
                        );
                    }
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
    }
}

fn seconds_remaining(deadline: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
}

fn update_dashboard_title(colors: bool, hint: &str, columns: Option<u16>) {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        SavePosition,
        MoveTo(0, 0),
        Clear(ClearType::CurrentLine),
        Print(dashboard_title_with_hint(colors, Some(hint), columns)),
        RestorePosition
    );
}

struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

enum WaitAction {
    Exit,
    Resize,
    Timeout,
}

fn wait_for_action(timeout: Duration, read_keys: bool) -> WaitAction {
    if !read_keys {
        thread::sleep(timeout);
        return WaitAction::Timeout;
    }

    let deadline = Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return WaitAction::Timeout;
        };
        if !event::poll(remaining).unwrap_or(false) {
            return WaitAction::Timeout;
        }
        match event::read() {
            Ok(Event::Key(key)) if is_exit_key(key) => return WaitAction::Exit,
            Ok(event) if is_resize_event(&event) => return WaitAction::Resize,
            _ => {}
        }
    }
}

fn is_exit_key(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('q'), KeyModifiers::NONE)
                | (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL)
        )
}

fn is_resize_event(event: &Event) -> bool {
    matches!(event, Event::Resize(_, _))
}

fn print_once(args: &DisplayArgs) -> i32 {
    let snapshot = fetch_snapshot(&args.query.providers);
    let terminal = io::stdout().is_terminal();
    let colors = terminal && !args.no_color && env::var_os("NO_COLOR").is_none();
    let columns = terminal
        .then(size)
        .and_then(Result::ok)
        .map(|(columns, _)| columns);
    let layout = dashboard_layout(&snapshot, args.compact, columns);
    println!("{}", render_dashboard(&snapshot, colors, layout));
    exit_code(&snapshot)
}

fn print_json(args: &QueryArgs) -> i32 {
    let snapshot = fetch_snapshot(&args.providers);
    println!(
        "{}",
        serde_json::to_string_pretty(&snapshot_view(&snapshot)).expect("snapshot serializes")
    );
    exit_code(&snapshot)
}

#[derive(Clone, Copy)]
struct DashboardLayout {
    compact: bool,
    bar_width: usize,
}

fn dashboard_layout(
    snapshot: &Snapshot,
    force_compact: bool,
    columns: Option<u16>,
) -> DashboardLayout {
    let reset_width = dashboard_reset_width(snapshot);
    let full_fixed_width = full_width(reset_width, 0);
    let compact_fixed_width = compact_width(reset_width, 0);
    let compact = force_compact
        || columns
            .is_some_and(|columns| usize::from(columns) < full_fixed_width + BAR_WIDTH_COMPACT);
    let (fixed_width, default_bar_width) = if compact {
        (compact_fixed_width, BAR_WIDTH_COMPACT)
    } else {
        (full_fixed_width, BAR_WIDTH)
    };
    let bar_width = columns.map_or(default_bar_width, |columns| {
        usize::from(columns).saturating_sub(fixed_width).max(1)
    });

    DashboardLayout { compact, bar_width }
}

fn exit_code(snapshot: &Snapshot) -> i32 {
    i32::from(!snapshot.providers.iter().any(|provider| provider.available))
}

fn fetch_snapshot(requested: &[Provider]) -> Snapshot {
    let providers = if requested.is_empty() {
        vec![Provider::Codex, Provider::OpencodeGo, Provider::ClaudeCode]
    } else {
        requested.to_vec()
    };
    let fetched_at = now();
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client builds");

    Snapshot {
        schema_version: JSON_SCHEMA_VERSION,
        fetched_at,
        providers: providers
            .into_iter()
            .map(|provider| match provider {
                Provider::Codex => fetch_codex(&client, fetched_at),
                Provider::OpencodeGo => fetch_opencode_go(&client, fetched_at),
                Provider::ClaudeCode => fetch_claude_code(&client, fetched_at),
            })
            .collect(),
    }
}

fn unavailable(provider: Provider, error: impl Into<String>, fetched_at: u64) -> ProviderUsage {
    ProviderUsage {
        provider: provider.name(),
        status: UsageStatus::Unavailable,
        available: false,
        plan: None,
        source: None,
        windows: vec![],
        error: Some(error.into()),
        fetched_at,
    }
}

fn fetch_codex(client: &Client, fetched_at: u64) -> ProviderUsage {
    let Some((access, account_id)) = codex_credentials() else {
        return unavailable(
            Provider::Codex,
            "Codex OAuth credential not found",
            fetched_at,
        );
    };
    let mut request = client
        .get(CODEX_USAGE_URL)
        .header("Accept", "application/json")
        .bearer_auth(access);
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let response = match request.send() {
        Ok(response) if response.status().is_success() => response,
        Ok(response) if response.status().as_u16() == 401 || response.status().as_u16() == 403 => {
            return unavailable(
                Provider::Codex,
                "Codex OAuth credential was rejected",
                fetched_at,
            );
        }
        Ok(_) | Err(_) => {
            return unavailable(Provider::Codex, "Codex usage request failed", fetched_at);
        }
    };
    let data: Value = match response.json() {
        Ok(data) => data,
        Err(_) => {
            return unavailable(
                Provider::Codex,
                "Codex usage response was invalid",
                fetched_at,
            );
        }
    };
    let windows = codex_windows(&data);
    if windows.is_empty() {
        return unavailable(
            Provider::Codex,
            "Codex usage windows unavailable",
            fetched_at,
        );
    }
    ProviderUsage {
        provider: Provider::Codex.name(),
        status: provider_status(&windows),
        available: true,
        plan: data
            .get("plan_type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        source: Some("oauth"),
        windows,
        error: None,
        fetched_at,
    }
}

fn codex_windows(data: &Value) -> Vec<UsageWindow> {
    let mut windows = [
        codex_window(
            data.pointer("/rate_limit/primary_window")
                .or_else(|| data.get("primary_window")),
            "5h",
            data,
        ),
        codex_window(
            data.pointer("/rate_limit/secondary_window")
                .or_else(|| data.get("secondary_window")),
            "7d",
            data,
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !windows.iter().any(|window| window.label == "5h") {
        windows.insert(0, unavailable_window("5h"));
    }
    windows
}

fn codex_window(raw: Option<&Value>, fallback: &'static str, data: &Value) -> Option<UsageWindow> {
    let raw = raw?;
    let used_percent = raw.get("used_percent")?.as_f64()?.clamp(0.0, 100.0);
    let window_seconds = raw.get("limit_window_seconds").and_then(Value::as_u64);
    let limit_reached = data
        .get("limit_reached")
        .and_then(Value::as_bool)
        .or_else(|| {
            data.pointer("/rate_limit/limit_reached")
                .and_then(Value::as_bool)
        })
        .or_else(|| raw.get("limit_reached").and_then(Value::as_bool));
    Some(UsageWindow {
        label: window_label(window_seconds, fallback),
        status: window_status(used_percent, limit_reached),
        used_percent: Some(used_percent),
        reset_at: raw.get("reset_at").and_then(Value::as_u64),
        window_seconds,
        limit_reached,
    })
}

fn unavailable_window(label: &'static str) -> UsageWindow {
    UsageWindow {
        label,
        status: UsageStatus::Unavailable,
        used_percent: None,
        reset_at: None,
        window_seconds: None,
        limit_reached: None,
    }
}

fn window_label(seconds: Option<u64>, fallback: &'static str) -> &'static str {
    match seconds {
        Some(18_000) => "5h",
        Some(604_800) => "7d",
        Some(2_592_000) => "30d",
        _ => fallback,
    }
}

fn codex_credentials() -> Option<(String, Option<String>)> {
    if let Ok(access) = env::var("LLM_USAGE_CODEX_ACCESS_TOKEN")
        && !access.trim().is_empty()
    {
        return Some((
            access,
            env::var("LLM_USAGE_CODEX_ACCOUNT_ID")
                .ok()
                .filter(|id| !id.trim().is_empty()),
        ));
    }
    for path in codex_auth_paths() {
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let mut content = String::new();
        if file.read_to_string(&mut content).is_err() {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if let Some(credentials) = pi_codex_credentials(&data) {
            return Some(credentials);
        }
        if let Some(credentials) = codex_cli_credentials(&data) {
            return Some(credentials);
        }
    }
    None
}

fn codex_auth_paths() -> Vec<PathBuf> {
    let mut paths = vec![];
    if let Some(path) = env::var_os("LLM_USAGE_CODEX_AUTH_FILE") {
        paths.push(PathBuf::from(path));
    }
    if let Some(agent_dir) = agent_dir() {
        paths.push(agent_dir.join("auth.json"));
    }
    if let Some(home) = home_dir() {
        paths.push(home.join(".pi/auth.json"));
        paths.push(home.join(".codex/auth.json"));
    }
    paths
}

fn agent_dir() -> Option<PathBuf> {
    let path = env::var_os("PI_CODING_AGENT_DIR")?;
    let path = PathBuf::from(path);
    if path.as_path() == Path::new("~") {
        return home_dir();
    }
    if let Some(relative) = path.to_string_lossy().strip_prefix("~/") {
        return home_dir().map(|home| home.join(relative));
    }
    Some(path)
}

fn pi_codex_credentials(data: &Value) -> Option<(String, Option<String>)> {
    let credential = data.get("openai-codex")?;
    if credential.get("type")?.as_str()? != "oauth" {
        return None;
    }
    let access = credential.get("access")?.as_str()?.trim();
    if access.is_empty() {
        return None;
    }
    Some((
        access.to_owned(),
        credential
            .get("accountId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    ))
}

fn codex_cli_credentials(data: &Value) -> Option<(String, Option<String>)> {
    let tokens = data.get("tokens")?;
    let access = tokens.get("access_token")?.as_str()?.trim();
    if access.is_empty() {
        return None;
    }
    Some((
        access.to_owned(),
        tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    ))
}

fn fetch_claude_code(client: &Client, fetched_at: u64) -> ProviderUsage {
    let Some((access_token, plan)) = claude_code_credentials() else {
        return unavailable(
            Provider::ClaudeCode,
            "Claude Code OAuth credential not found",
            fetched_at,
        );
    };
    let cache_path = claude_code_cache_path();
    let cache = cache_path.as_deref().and_then(read_claude_code_cache);
    if let Some(cache) = cache.as_ref()
        && claude_code_cache_throttled(cache, fetched_at)
    {
        let remaining = cache.refresh_after - fetched_at;
        let reason = match cache.cooldown {
            Some(ClaudeCodeCooldown::RateLimited) => "Claude Code usage is rate limited",
            Some(ClaudeCodeCooldown::Transient) => "Claude Code usage is temporarily unavailable",
            None => "Claude Code usage refresh is throttled",
        };
        let error = format!("{reason}; retry in {}", short_duration(remaining));
        return cached_claude_code_usage(cache, fetched_at, &error)
            .unwrap_or_else(|| unavailable(Provider::ClaudeCode, error, fetched_at));
    }

    let data = match request_claude_code_usage(client, &access_token) {
        Ok(data) => data,
        Err(ClaudeCodeRequestError::Authentication) => {
            return unavailable(
                Provider::ClaudeCode,
                "Claude Code OAuth credential was rejected",
                fetched_at,
            );
        }
        Err(ClaudeCodeRequestError::Retry {
            delay,
            cooldown,
            message,
        }) => {
            return failed_claude_code_refresh(
                cache,
                cache_path.as_deref(),
                fetched_at,
                delay,
                cooldown,
                &message,
            );
        }
    };
    let windows = claude_code_windows(&data);
    if windows.is_empty() {
        return failed_claude_code_refresh(
            cache,
            cache_path.as_deref(),
            fetched_at,
            CLAUDE_CODE_REFRESH_INTERVAL,
            ClaudeCodeCooldown::Transient,
            "Claude Code usage windows unavailable",
        );
    }
    let usage = ProviderUsage {
        provider: Provider::ClaudeCode.name(),
        status: provider_status(&windows),
        available: true,
        plan: plan.or_else(|| {
            data.get("plan")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        }),
        source: Some("oauth"),
        windows,
        error: None,
        fetched_at,
    };
    if let Some(path) = cache_path.as_deref() {
        let cache = ClaudeCodeCache::from_usage(
            &usage,
            fetched_at.saturating_add(CLAUDE_CODE_REFRESH_INTERVAL),
        );
        let _ = write_claude_code_cache(path, &cache);
    }
    usage
}

fn request_claude_code_usage(
    client: &Client,
    access_token: &str,
) -> Result<Value, ClaudeCodeRequestError> {
    let response = client
        .get(CLAUDE_CODE_USAGE_URL)
        .header("Accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .bearer_auth(access_token)
        .send()
        .map_err(|_| ClaudeCodeRequestError::Retry {
            delay: CLAUDE_CODE_REFRESH_INTERVAL,
            cooldown: ClaudeCodeCooldown::Transient,
            message: CLAUDE_CODE_REQUEST_ERROR.to_owned(),
        })?;
    let status = response.status();
    if claude_code_auth_failure(status) {
        return Err(ClaudeCodeRequestError::Authentication);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let delay = claude_code_retry_after(response.headers().get(RETRY_AFTER), Utc::now())
            .unwrap_or(CLAUDE_CODE_RATE_LIMIT_COOLDOWN)
            .max(60);
        return Err(ClaudeCodeRequestError::Retry {
            delay,
            cooldown: ClaudeCodeCooldown::RateLimited,
            message: "Claude Code usage is rate limited (HTTP 429)".to_owned(),
        });
    }
    if !status.is_success() {
        return Err(ClaudeCodeRequestError::Retry {
            delay: CLAUDE_CODE_REFRESH_INTERVAL,
            cooldown: ClaudeCodeCooldown::Transient,
            message: format!("{CLAUDE_CODE_REQUEST_ERROR} (HTTP {})", status.as_u16()),
        });
    }
    response.json().map_err(|_| ClaudeCodeRequestError::Retry {
        delay: CLAUDE_CODE_REFRESH_INTERVAL,
        cooldown: ClaudeCodeCooldown::Transient,
        message: "Claude Code usage response was invalid".to_owned(),
    })
}

impl ClaudeCodeCache {
    fn empty() -> Self {
        Self {
            schema_version: CLAUDE_CODE_CACHE_SCHEMA_VERSION,
            fetched_at: 0,
            refresh_after: 0,
            cooldown: None,
            plan: None,
            windows: Vec::new(),
        }
    }

    fn from_usage(usage: &ProviderUsage, refresh_after: u64) -> Self {
        Self {
            schema_version: CLAUDE_CODE_CACHE_SCHEMA_VERSION,
            fetched_at: usage.fetched_at,
            refresh_after,
            cooldown: None,
            plan: usage.plan.clone(),
            windows: usage
                .windows
                .iter()
                .map(|window| CachedUsageWindow {
                    label: window.label.to_owned(),
                    status: window.status,
                    used_percent: window.used_percent,
                    reset_at: window.reset_at,
                    window_seconds: window.window_seconds,
                    limit_reached: window.limit_reached,
                })
                .collect(),
        }
    }
}

fn claude_code_auth_failure(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

fn claude_code_cache_throttled(cache: &ClaudeCodeCache, now: u64) -> bool {
    cache.refresh_after > now
}

fn failed_claude_code_refresh(
    cache: Option<ClaudeCodeCache>,
    cache_path: Option<&Path>,
    fetched_at: u64,
    retry_after: u64,
    cooldown: ClaudeCodeCooldown,
    error: &str,
) -> ProviderUsage {
    let mut cache = cache.unwrap_or_else(ClaudeCodeCache::empty);
    cache.refresh_after = fetched_at.saturating_add(retry_after);
    cache.cooldown = Some(cooldown);
    if let Some(path) = cache_path {
        let _ = write_claude_code_cache(path, &cache);
    }
    let error = format!("{error}; retry in {}", short_duration(retry_after));
    cached_claude_code_usage(&cache, fetched_at, &error)
        .unwrap_or_else(|| unavailable(Provider::ClaudeCode, error, fetched_at))
}

fn cached_claude_code_usage(
    cache: &ClaudeCodeCache,
    now: u64,
    error: &str,
) -> Option<ProviderUsage> {
    let age = now.saturating_sub(cache.fetched_at);
    if cache.windows.is_empty() || age > CLAUDE_CODE_CACHE_MAX_AGE {
        return None;
    }
    let windows = cache
        .windows
        .iter()
        .filter_map(|window| {
            let label = match window.label.as_str() {
                "5h" => "5h",
                "7d" => "7d",
                _ => return None,
            };
            Some(UsageWindow {
                label,
                status: window.status,
                used_percent: window.used_percent,
                reset_at: window.reset_at,
                window_seconds: window.window_seconds,
                limit_reached: window.limit_reached,
            })
        })
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    Some(ProviderUsage {
        provider: Provider::ClaudeCode.name(),
        status: UsageStatus::Stale,
        available: true,
        plan: cache.plan.clone(),
        source: Some("oauth"),
        windows,
        error: Some(format!(
            "{error}; cached data is {} old",
            short_duration(age)
        )),
        fetched_at: cache.fetched_at,
    })
}

fn claude_code_cache_path() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".cache")))
        .map(|root| root.join("llm-usage/claude-code.json"))
}

fn read_claude_code_cache(path: &Path) -> Option<ClaudeCodeCache> {
    let cache = serde_json::from_reader::<_, ClaudeCodeCache>(File::open(path).ok()?).ok()?;
    (cache.schema_version == CLAUDE_CODE_CACHE_SCHEMA_VERSION).then_some(cache)
}

fn write_claude_code_cache(path: &Path, cache: &ClaudeCodeCache) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec(cache).map_err(io::Error::other)?,
    )?;
    fs::rename(temporary, path)
}

fn claude_code_retry_after(value: Option<&HeaderValue>, now: DateTime<Utc>) -> Option<u64> {
    let value = value?.to_str().ok()?;
    if let Ok(seconds) = value.parse() {
        return Some(seconds);
    }
    let retry_at = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    u64::try_from((retry_at - now).num_seconds().max(0)).ok()
}

fn short_duration(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if seconds == 0 {
        format!("{minutes}m")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn claude_code_windows(data: &Value) -> Vec<UsageWindow> {
    let mut windows = [
        claude_code_window(data.get("five_hour"), "5h", 18_000),
        claude_code_window(data.get("seven_day"), "7d", 604_800),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !windows.iter().any(|window| window.label == "5h") {
        windows.insert(0, unavailable_window("5h"));
    }
    windows
}

fn claude_code_window(
    raw: Option<&Value>,
    label: &'static str,
    seconds: u64,
) -> Option<UsageWindow> {
    let raw = raw?;
    let used_percent = raw.get("utilization")?.as_f64()?.clamp(0.0, 100.0);
    let reset_at = raw
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| u64::try_from(dt.with_timezone(&Utc).timestamp()).unwrap_or(0));
    Some(UsageWindow {
        label,
        status: window_status(used_percent, None),
        used_percent: Some(used_percent),
        reset_at,
        window_seconds: Some(seconds),
        limit_reached: None,
    })
}

fn claude_code_credentials() -> Option<(String, Option<String>)> {
    if let Ok(access) = env::var("LLM_USAGE_CLAUDE_CODE_ACCESS_TOKEN")
        && !access.trim().is_empty()
    {
        return Some((access, None));
    }
    for path in claude_code_auth_paths() {
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let mut content = String::new();
        if file.read_to_string(&mut content).is_err() {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if let Some(creds) = claude_code_oauth_credentials(&data) {
            return Some(creds);
        }
    }
    if let Some(creds) = claude_code_keychain_credentials() {
        return Some(creds);
    }
    None
}

fn claude_code_keychain_credentials() -> Option<(String, Option<String>)> {
    if cfg!(not(target_os = "macos")) {
        return None;
    }

    // Claude Code may store one token per organization under services like
    // `Claude Code-credentials-<org-hash>`, plus a legacy `Claude Code-credentials`
    // entry. Enumerate every matching service via `security dump-keychain`, then
    // return the freshest unexpired token.
    let dump = std::process::Command::new("security")
        .args(["dump-keychain"])
        .output()
        .ok()?;
    let dump_str = String::from_utf8_lossy(&dump.stdout);
    let services: Vec<String> = dump_str
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("\"svce\"<blob>=\""))
        .filter_map(|line| {
            let after = line
                .strip_prefix("\"svce\"<blob>=\"")
                .and_then(|rest| rest.strip_suffix('"'))?;
            after
                .strip_prefix("Claude Code-credentials")
                .map(|_| after.to_string())
        })
        .collect();
    if services.is_empty() {
        return None;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0);
    let mut best: Option<(u64, (String, Option<String>))> = None;
    for service in &services {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service.as_str(), "-w"])
            .output()
            .ok()?;
        // Skip prompts / failures rather than aborting enumeration.
        if !output.status.success() {
            continue;
        }
        let content = String::from_utf8_lossy(&output.stdout);
        let Ok(data) = serde_json::from_str::<Value>(content.trim()) else {
            continue;
        };
        let Some(creds) = claude_code_oauth_credentials(&data) else {
            continue;
        };
        let expires_at = data
            .get("claudeAiOauth")
            .and_then(|o| o.get("expiresAt"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if expires_at > now_ms && (best.is_none() || expires_at > best.as_ref().unwrap().0) {
            best = Some((expires_at, creds));
        }
    }
    best.map(|(_, creds)| creds)
}

fn claude_code_auth_paths() -> Vec<PathBuf> {
    let mut paths = vec![];
    if let Some(path) = env::var_os("LLM_USAGE_CLAUDE_CODE_AUTH_FILE") {
        paths.push(PathBuf::from(path));
    }
    if let Some(home) = home_dir() {
        paths.push(home.join(".claude/.credentials.json"));
    }
    paths
}

fn claude_code_oauth_credentials(data: &Value) -> Option<(String, Option<String>)> {
    let oauth = data.get("claudeAiOauth")?;
    let access = oauth.get("accessToken")?.as_str()?.trim();
    if access.is_empty() {
        return None;
    }
    let plan = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Some((access.to_owned(), plan))
}

fn fetch_opencode_go(client: &Client, fetched_at: u64) -> ProviderUsage {
    let Some(api_key) = nonempty_env("OPENCODE_GO_API_KEY") else {
        return unavailable(Provider::OpencodeGo, "set OPENCODE_GO_API_KEY", fetched_at);
    };
    let response = match client
        .get(OPENCODE_GO_USAGE_URL)
        .bearer_auth(api_key)
        .send()
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) if response.status().as_u16() == 401 => {
            return unavailable(
                Provider::OpencodeGo,
                "OpenCode Go API authentication failed",
                fetched_at,
            );
        }
        Ok(response) if response.status().as_u16() == 403 => {
            return unavailable(
                Provider::OpencodeGo,
                "OpenCode Go subscription required",
                fetched_at,
            );
        }
        Ok(_) | Err(_) => {
            return unavailable(
                Provider::OpencodeGo,
                "OpenCode Go API request failed",
                fetched_at,
            );
        }
    };
    let Ok(data) = response.json::<Value>() else {
        return unavailable(
            Provider::OpencodeGo,
            "OpenCode Go API response was invalid",
            fetched_at,
        );
    };
    let windows = opencode_api_windows(&data);
    if windows.is_empty() {
        return unavailable(
            Provider::OpencodeGo,
            "OpenCode Go usage was not found",
            fetched_at,
        );
    }
    ProviderUsage {
        provider: Provider::OpencodeGo.name(),
        status: provider_status(&windows),
        available: true,
        plan: Some("Go".to_owned()),
        source: Some("api"),
        windows,
        error: None,
        fetched_at,
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn opencode_api_windows(data: &Value) -> Vec<UsageWindow> {
    let usage = data.get("usage");
    [
        (usage.and_then(|u| u.get("rolling")), "5h", 18_000),
        (usage.and_then(|u| u.get("weekly")), "7d", 604_800),
        (usage.and_then(|u| u.get("monthly")), "30d", 2_592_000),
    ]
    .into_iter()
    .filter_map(|(value, label, seconds)| opencode_api_window(value, label, seconds))
    .collect()
}

fn opencode_api_window(
    value: Option<&Value>,
    label: &'static str,
    seconds: u64,
) -> Option<UsageWindow> {
    let value = value?;
    let percent = value.get("percent").and_then(Value::as_f64)?;
    let reset_iso = value.get("resetsAt").and_then(Value::as_str)?;
    let reset_at = u64::try_from(DateTime::parse_from_rfc3339(reset_iso).ok()?.timestamp()).ok()?;
    let limit_reached = value
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s == "rate-limited");
    let used_percent = percent.clamp(0.0, 100.0);
    Some(UsageWindow {
        label,
        status: window_status(used_percent, limit_reached),
        used_percent: Some(used_percent),
        reset_at: Some(reset_at),
        window_seconds: Some(seconds),
        limit_reached,
    })
}

fn window_status(used_percent: f64, limit_reached: Option<bool>) -> UsageStatus {
    if limit_reached == Some(true) || used_percent >= 100.0 {
        UsageStatus::RateLimited
    } else {
        UsageStatus::Ok
    }
}

fn provider_status(windows: &[UsageWindow]) -> UsageStatus {
    if windows
        .iter()
        .any(|window| window.status == UsageStatus::RateLimited)
    {
        UsageStatus::RateLimited
    } else {
        UsageStatus::Ok
    }
}

fn provider_label(provider: &'static str) -> &'static str {
    match provider {
        "codex" => Provider::Codex.label(),
        "opencode-go" => Provider::OpencodeGo.label(),
        "claude-code" => Provider::ClaudeCode.label(),
        other => other,
    }
}

fn provider_summary_label(provider: &'static str) -> &'static str {
    match provider {
        "codex" => "Codex",
        "opencode-go" => "Go",
        "claude-code" => "Claude",
        other => other,
    }
}

/// Adds derived quota state to a snapshot without changing any raw field.
fn snapshot_view(snapshot: &Snapshot) -> SnapshotView<'_> {
    let providers: Vec<ProviderView<'_>> = snapshot
        .providers
        .iter()
        .map(|usage| ProviderView {
            display: provider_display(usage),
            usage,
        })
        .collect();

    let best_available = best_available(&providers);
    SnapshotView {
        schema_version: snapshot.schema_version,
        fetched_at: snapshot.fetched_at,
        presentation: presentation(&providers, best_available, snapshot.fetched_at),
        best_available,
        providers,
    }
}

fn provider_display(usage: &ProviderUsage) -> ProviderDisplay {
    let exhausted = usage.status == UsageStatus::RateLimited;
    let limiting = limiting_window(&usage.windows);
    let capacity_used_percent = if usage.status == UsageStatus::Unavailable {
        None
    } else if exhausted {
        Some(100)
    } else {
        limiting.map(|window| rounded_percent(window.used_percent.unwrap_or_default()))
    };

    ProviderDisplay {
        name: provider_label(usage.provider),
        exhausted,
        capacity_used_percent,
        windows: usage
            .windows
            .iter()
            .filter(|window| window.status != UsageStatus::Unavailable)
            .filter(|window| window.used_percent.is_some())
            .map(display_window)
            .collect(),
        limiting_window: limiting.map(display_window),
        next_reset_at: next_reset_at(&usage.windows),
    }
}

fn display_window(window: &UsageWindow) -> DisplayWindow {
    DisplayWindow {
        label: window.label,
        used_percent: rounded_percent(window.used_percent.unwrap_or_default()),
        reset_at: window.reset_at,
    }
}

/// Picks the window that constrains a provider: a rate-limited window first,
/// then the highest usage. Ties keep response order, so the shortest window
/// wins because providers list windows shortest first.
fn limiting_window(windows: &[UsageWindow]) -> Option<&UsageWindow> {
    fn rank(window: &UsageWindow) -> (bool, f64) {
        (
            window.status == UsageStatus::RateLimited,
            window.used_percent.unwrap_or_default(),
        )
    }

    let mut limiting: Option<&UsageWindow> = None;
    for window in windows
        .iter()
        .filter(|window| window.status != UsageStatus::Unavailable)
        .filter(|window| window.used_percent.is_some())
    {
        limiting = match limiting {
            Some(current) if rank(current) >= rank(window) => Some(current),
            _ => Some(window),
        };
    }
    limiting
}

/// Soonest reset among windows that report one, ignoring unavailable windows.
fn next_reset_at(windows: &[UsageWindow]) -> Option<u64> {
    windows
        .iter()
        .filter(|window| window.status != UsageStatus::Unavailable)
        .filter_map(|window| window.reset_at)
        .min()
}

/// Lowest-usage provider that is not unavailable. Providers without a
/// comparable capacity (no usable window) are skipped, and ties keep snapshot
/// order.
fn best_available(providers: &[ProviderView<'_>]) -> Option<BestAvailable> {
    let mut best: Option<BestAvailable> = None;
    for view in providers {
        if view.usage.status == UsageStatus::Unavailable {
            continue;
        }
        let Some(capacity_used_percent) = view.display.capacity_used_percent else {
            continue;
        };
        let candidate = BestAvailable {
            provider: view.usage.provider,
            capacity_used_percent,
        };
        best = match best {
            Some(current) if current.capacity_used_percent <= capacity_used_percent => {
                Some(current)
            }
            _ => Some(candidate),
        };
    }
    best
}

/// Builds compact status text for widgets. Severity stays semantic and every
/// raw and derived value remains available alongside it.
fn presentation(
    providers: &[ProviderView<'_>],
    best: Option<BestAvailable>,
    fetched_at: u64,
) -> Presentation {
    let summary = providers
        .iter()
        .filter(|view| presentation_visible(view))
        .map(|view| {
            format!(
                "{} {}",
                provider_summary_label(view.usage.provider),
                presentation_status(view, fetched_at)
            )
        })
        .collect::<Vec<String>>()
        .join(" · ");

    Presentation {
        summary: if summary.is_empty() {
            "no usage data".to_owned()
        } else {
            summary
        },
        severity: severity(best),
        providers: providers
            .iter()
            .map(|view| ProviderPresentation {
                provider: view.usage.provider,
                label: format!(
                    "{:<PROVIDER_WIDTH$} {}",
                    view.display.name,
                    presentation_windows_status(view, fetched_at)
                ),
                visible: presentation_visible(view),
            })
            .collect(),
        freshness: freshness_text(&local_clock(fetched_at)),
    }
}

/// Compact status for one provider: `42% 5h ↻3h`, `unavailable`, or
/// `no usage data`.
fn presentation_status(view: &ProviderView<'_>, fetched_at: u64) -> String {
    if view.usage.status == UsageStatus::Unavailable {
        return "unavailable".to_owned();
    }
    let Some(percent) = view.display.capacity_used_percent else {
        return "no usage data".to_owned();
    };

    let window = view
        .display
        .limiting_window
        .as_ref()
        .map_or_else(String::new, |window| {
            let reset = window.reset_at.map_or_else(String::new, |reset_at| {
                format!(" ↻{}", compact_reset(reset_at, fetched_at))
            });
            format!(" {}{reset}", window.label)
        });
    let stale = stale_suffix(view);

    format!("{percent}%{window}{stale}")
}

/// Status for one provider listing every usable window:
/// `42% 5h ↻2h · 12% 7d ↻3d`. Falls back to the compact status when no
/// window is usable.
fn presentation_windows_status(view: &ProviderView<'_>, fetched_at: u64) -> String {
    if view.usage.status == UsageStatus::Unavailable
        || view.display.capacity_used_percent.is_none()
        || view.display.windows.is_empty()
    {
        return presentation_status(view, fetched_at);
    }

    let windows = view
        .display
        .windows
        .iter()
        .map(|window| {
            let reset = window.reset_at.map_or_else(String::new, |reset_at| {
                format!(" ↻{}", compact_reset(reset_at, fetched_at))
            });
            format!("{}% {}{reset}", window.used_percent, window.label)
        })
        .collect::<Vec<String>>()
        .join(" · ");
    let stale = stale_suffix(view);

    format!("{windows}{stale}")
}

/// ` ≈` for a stale provider, empty otherwise. Suffixed so the percentage
/// keeps a predictable position for consumers that truncate from the right.
fn stale_suffix(view: &ProviderView<'_>) -> String {
    if view.usage.status == UsageStatus::Stale {
        format!(" {STALE_MARK}")
    } else {
        String::new()
    }
}

/// A provider is worth showing in a compact widget once it reports usable
/// capacity. Fuller UIs can still render every entry.
fn presentation_visible(view: &ProviderView<'_>) -> bool {
    view.usage.status != UsageStatus::Unavailable && view.display.capacity_used_percent.is_some()
}

/// Semantic severity of the best available provider, using the same
/// thresholds as the terminal dashboard colors. Consumers choose colors.
fn severity(best: Option<BestAvailable>) -> Severity {
    match best {
        None => Severity::Unknown,
        Some(best) if best.capacity_used_percent >= CRITICAL_PERCENT => Severity::Critical,
        Some(best) if best.capacity_used_percent >= WARNING_PERCENT => Severity::Warning,
        Some(_) => Severity::Ok,
    }
}

/// Largest whole unit only, so status text stays short: `7d`, `3h`, `12m`.
fn compact_reset(reset_at: u64, now: u64) -> String {
    let seconds = reset_at.saturating_sub(now);
    if seconds == 0 {
        return "now".to_owned();
    }
    let days = seconds / 86_400;
    let hours = seconds / 3_600;
    let minutes = seconds / 60;
    if days > 0 {
        format!("{days}d")
    } else if hours > 0 {
        format!("{hours}h")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn freshness_text(clock: &str) -> String {
    format!("Updated {clock} · ↻ = reset in · {STALE_MARK} = stale")
}

fn local_clock(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map_or_else(
            || "unknown".to_owned(),
            |utc| utc.with_timezone(&Local).format("%H:%M:%S").to_string(),
        )
}

fn dashboard_reset_width(snapshot: &Snapshot) -> usize {
    snapshot
        .providers
        .iter()
        .flat_map(|provider| provider.windows.iter())
        .filter_map(|window| {
            window
                .reset_at
                .map(|reset_at| reset_text(Some(reset_at), snapshot.fetched_at))
        })
        .map(|text| text.chars().count())
        .fold(RESET_WIDTH, usize::max)
}

fn render_dashboard(snapshot: &Snapshot, colors: bool, layout: DashboardLayout) -> String {
    render_dashboard_with_hint(snapshot, colors, layout, None, None)
}

#[allow(clippy::too_many_lines)]
fn render_dashboard_with_hint(
    snapshot: &Snapshot,
    colors: bool,
    layout: DashboardLayout,
    refresh_hint: Option<&str>,
    columns: Option<u16>,
) -> String {
    let reset_width = dashboard_reset_width(snapshot);
    let DashboardLayout { compact, bar_width } = layout;
    let mut lines = vec![dashboard_title_with_hint(colors, refresh_hint, columns)];
    let mut rendered_header = false;
    for (index, provider) in snapshot.providers.iter().enumerate() {
        if index > 0 {
            lines.push(provider_separator(colors, reset_width, compact, bar_width));
        }
        let label = provider_label(provider.provider);
        if !provider.available {
            if compact {
                lines.push(format!(" {label}"));
                lines.push(format!(
                    "   unavailable: {}",
                    provider.error.as_deref().unwrap_or("usage unavailable")
                ));
            } else {
                lines.push(format!(
                    " {:<PROVIDER_WIDTH$} unavailable: {}",
                    label,
                    provider.error.as_deref().unwrap_or("usage unavailable")
                ));
            }
            continue;
        }
        if !rendered_header
            && provider
                .windows
                .iter()
                .any(|window| window.used_percent.is_some())
            && !compact
        {
            lines.push(header_text(colors, reset_width, bar_width));
            lines.push(provider_separator(colors, reset_width, false, bar_width));
            rendered_header = true;
        }
        if compact {
            lines.push(format!(" {label}"));
        }
        if provider.status == UsageStatus::Stale {
            let error = provider.error.as_deref().unwrap_or("cached usage is stale");
            if compact {
                lines.push(format!("   stale: {error}"));
            } else {
                lines.push(format!(" {label:<PROVIDER_WIDTH$} stale: {error}"));
            }
        }
        for window in &provider.windows {
            let Some(used_percent) = window.used_percent else {
                let bar = usage_bar(0, bar_width);
                let bar = if colors { muted_text(&bar) } else { bar };
                if compact {
                    lines.push(format!(
                        "   {:>COMPACT_WINDOW_WIDTH$} [{}] {:>3}   {:>reset_width$}",
                        window.label, bar, "", "unavailable"
                    ));
                } else {
                    lines.push(format!(
                        " {:<PROVIDER_WIDTH$} {:>WINDOW_WIDTH$} [{}] {:>3}   {:>reset_width$}",
                        label, window.label, bar, "", "unavailable"
                    ));
                }
                continue;
            };
            let percent = rounded_percent(used_percent);
            let bar = colored_usage_bar(percent, bar_width, colors);
            let percent = usage_percent(percent, colors);
            if compact {
                lines.push(format!(
                    "   {:>COMPACT_WINDOW_WIDTH$} [{}] {}  {:>reset_width$}",
                    window.label,
                    bar,
                    percent,
                    reset_text(window.reset_at, snapshot.fetched_at)
                ));
            } else {
                lines.push(format!(
                    " {:<PROVIDER_WIDTH$} {:>WINDOW_WIDTH$} [{}] {}  {:>reset_width$}",
                    label,
                    window.label,
                    bar,
                    percent,
                    reset_text(window.reset_at, snapshot.fetched_at)
                ));
            }
        }
    }
    lines.join("\n")
}

#[allow(dead_code)]
fn dashboard_title(colors: bool) -> String {
    dashboard_title_with_hint(colors, None, None)
}

fn dashboard_title_with_hint(
    colors: bool,
    refresh_hint: Option<&str>,
    columns: Option<u16>,
) -> String {
    const TITLE: &str = " LLM USAGE";
    const SUBTITLE: &str = " · subscription quotas";
    let refresh_hint = refresh_hint.unwrap_or_default();
    let full_title = format!("{TITLE}{SUBTITLE}{refresh_hint}");
    let max_width = columns.map(usize::from);
    let subtitle = if max_width.is_none_or(|width| full_title.chars().count() <= width) {
        SUBTITLE
    } else {
        ""
    };
    let concise_title = format!("{TITLE}{refresh_hint}");

    if max_width.is_some_and(|width| concise_title.chars().count() > width) {
        let compact = if refresh_hint.is_empty() {
            TITLE.trim_start().to_owned()
        } else {
            format!(
                "{} {}",
                refresh_hint.trim_start_matches(" · "),
                TITLE.trim_start()
            )
        };
        let compact = compact
            .chars()
            .take(max_width.unwrap_or_default())
            .collect::<String>();
        return if colors {
            muted_text(&compact)
        } else {
            compact
        };
    }

    if colors {
        format!(
            "{}{}{TITLE}{}{}{}",
            SetAttribute(Attribute::Bold),
            SetForegroundColor(Color::Blue),
            SetAttribute(Attribute::Reset),
            muted_text(subtitle),
            muted_text(refresh_hint)
        )
    } else {
        format!("{TITLE}{subtitle}{refresh_hint}")
    }
}

fn refresh_hint(interval: u64, remaining: u64, columns: Option<u16>) -> String {
    const MAX_DOTS: usize = 30;
    const FIXED_HINT: &str = " · refresh: [] · q to exit";

    let Some(columns) = columns else {
        return format!(" · refresh: {remaining}s · q to exit");
    };
    let fixed_width = dashboard_title_with_hint(false, Some(FIXED_HINT), None)
        .chars()
        .count();
    let width = usize::from(columns)
        .saturating_sub(fixed_width)
        .min(MAX_DOTS)
        .min(usize::try_from(interval).unwrap_or(MAX_DOTS));
    if width == 0 {
        return format!(" · {}", refresh_progress(interval, remaining));
    }

    let active = usize::try_from(
        (u128::from(remaining.min(interval)) * u128::try_from(width).unwrap_or_default())
            .div_ceil(u128::from(interval)),
    )
    .unwrap_or(width);
    format!(
        " · refresh: [{}{}] · q to exit",
        ".".repeat(active),
        " ".repeat(width - active)
    )
}

fn refresh_progress(interval: u64, remaining: u64) -> char {
    const FRAMES: [char; 9] = ['⠀', '⠁', '⠃', '⠇', '⠏', '⠟', '⠿', '⡿', '⣿'];

    let interval = interval.max(1);
    let level =
        usize::try_from((u128::from(remaining.min(interval)) * 8).div_ceil(u128::from(interval)))
            .unwrap_or_default();
    FRAMES[level]
}

fn colored_usage_bar(percent: u8, width: usize, colors: bool) -> String {
    if !colors {
        return usage_bar(percent, width);
    }

    let filled = (usize::from(percent.min(100)) * width + 50) / 100;
    let empty = width - filled;
    format!(
        "{}{}{}{}{}",
        SetForegroundColor(usage_color(percent)),
        "█".repeat(filled),
        SetForegroundColor(unused_color(percent)),
        "░".repeat(empty),
        ResetColor
    )
}

fn usage_percent(percent: u8, colors: bool) -> String {
    let text = format!("{percent:>3}%");
    if colors {
        color_text(&text, usage_color(percent))
    } else {
        text
    }
}

fn header_text(colors: bool, reset_width: usize, bar_width: usize) -> String {
    let usage_width = bar_width + 7;
    let header = format!(
        " {:<PROVIDER_WIDTH$} {:>WINDOW_WIDTH$} {:<usage_width$}  {:>reset_width$}",
        "provider", "meter", "usage", "resets in"
    );
    if colors { muted_text(&header) } else { header }
}

fn full_width(reset_width: usize, bar_width: usize) -> usize {
    header_text(false, reset_width, bar_width).chars().count()
}

fn compact_width(reset_width: usize, bar_width: usize) -> usize {
    3 + COMPACT_WINDOW_WIDTH + 1 + 2 + bar_width + 1 + 4 + 2 + reset_width
}

fn provider_separator(colors: bool, reset_width: usize, compact: bool, bar_width: usize) -> String {
    let width = if compact {
        compact_width(reset_width, bar_width)
    } else {
        full_width(reset_width, bar_width)
    };
    let separator = "─".repeat(width);
    if colors {
        muted_text(&separator)
    } else {
        separator
    }
}

fn muted_text(text: &str) -> String {
    color_text(text, Color::DarkGrey)
}

fn color_text(text: &str, color: Color) -> String {
    format!("{}{text}{}", SetForegroundColor(color), ResetColor)
}

fn rounded_percent(percent: f64) -> u8 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        percent.round().clamp(0.0, 100.0) as u8
    }
}

fn usage_bar(percent: u8, width: usize) -> String {
    let filled = (usize::from(percent.min(100)) * width + 50) / 100;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn usage_color(percent: u8) -> Color {
    if percent >= CRITICAL_PERCENT {
        Color::DarkRed
    } else if percent >= WARNING_PERCENT {
        Color::DarkYellow
    } else {
        Color::DarkGreen
    }
}

fn unused_color(percent: u8) -> Color {
    if percent >= CRITICAL_PERCENT {
        Color::Red
    } else if percent >= WARNING_PERCENT {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn reset_text(reset_at: Option<u64>, now: u64) -> String {
    let Some(reset_at) = reset_at else {
        return String::new();
    };
    let seconds = reset_at.saturating_sub(now);
    if seconds == 0 {
        return "now".to_owned();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_layout(compact: bool) -> DashboardLayout {
        DashboardLayout {
            compact,
            bar_width: if compact {
                BAR_WIDTH_COMPACT
            } else {
                BAR_WIDTH
            },
        }
    }

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: Vec::new(),
        }
    }

    fn sample_claude_usage(fetched_at: u64) -> ProviderUsage {
        ProviderUsage {
            provider: Provider::ClaudeCode.name(),
            status: UsageStatus::Ok,
            available: true,
            plan: Some("max".to_owned()),
            source: Some("oauth"),
            windows: vec![UsageWindow {
                label: "5h",
                status: UsageStatus::Ok,
                used_percent: Some(42.0),
                reset_at: Some(2_000),
                window_seconds: Some(18_000),
                limit_reached: None,
            }],
            error: None,
            fetched_at,
        }
    }

    #[test]
    fn claude_code_cache_is_durable_sanitized_and_expires_after_one_hour() {
        let path = env::temp_dir().join(format!(
            "llm-usage-claude-cache-test-{}.json",
            std::process::id()
        ));
        let cache = ClaudeCodeCache::from_usage(&sample_claude_usage(1_000), 1_300);

        write_claude_code_cache(&path, &cache).unwrap();
        let loaded = read_claude_code_cache(&path).unwrap();
        let serialized = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert!(cached_claude_code_usage(&loaded, 4_600, "stale").is_some());
        assert!(cached_claude_code_usage(&loaded, 4_601, "stale").is_none());
        assert!(!serialized.contains("access_token"));
        assert!(!serialized.contains("response"));
        assert!(!serialized.contains("oauth-token"));
    }

    #[test]
    fn claude_code_cache_throttles_refreshes_and_persists_rate_limit_cooldown() {
        let path = env::temp_dir().join(format!(
            "llm-usage-claude-rate-limit-test-{}.json",
            std::process::id()
        ));
        let cache = ClaudeCodeCache::from_usage(&sample_claude_usage(1_000), 1_300);
        assert!(claude_code_cache_throttled(&cache, 1_030));
        assert!(!claude_code_cache_throttled(&cache, 1_300));

        let stale = failed_claude_code_refresh(
            Some(cache),
            Some(&path),
            1_100,
            CLAUDE_CODE_RATE_LIMIT_COOLDOWN,
            ClaudeCodeCooldown::RateLimited,
            "Claude Code usage is rate limited (HTTP 429)",
        );
        let persisted = read_claude_code_cache(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(persisted.refresh_after, 2_000);
        assert_eq!(persisted.cooldown, Some(ClaudeCodeCooldown::RateLimited));
        assert!(stale.available);
        assert_eq!(stale.status, UsageStatus::Stale);
        assert_eq!(stale.fetched_at, 1_000);
        assert!(
            stale
                .error
                .as_deref()
                .is_some_and(|error| error.contains("retry in 15m") && error.contains("1m 40s old"))
        );

        let retry_after = HeaderValue::from_static("60");
        assert_eq!(
            claude_code_retry_after(Some(&retry_after), Utc::now()),
            Some(60)
        );
    }

    #[test]
    fn cached_claude_code_usage_renders_stale_age() {
        let cache = ClaudeCodeCache::from_usage(&sample_claude_usage(1_000), 1_300);
        let stale = cached_claude_code_usage(&cache, 1_100, "refresh throttled").unwrap();
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_100,
            providers: vec![stale],
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        let dashboard = render_dashboard(&snapshot, false, default_layout(false));

        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["providers"][0]["status"], "stale");
        assert!(dashboard.contains("stale:"));
        assert!(dashboard.contains("cached data is 1m 40s old"));
    }

    #[test]
    fn claude_code_auth_failures_remain_distinct() {
        assert!(claude_code_auth_failure(StatusCode::UNAUTHORIZED));
        assert!(claude_code_auth_failure(StatusCode::FORBIDDEN));
        assert!(!claude_code_auth_failure(StatusCode::TOO_MANY_REQUESTS));
        assert!(!claude_code_auth_failure(StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn layout_uses_full_view_when_it_fits() {
        let snapshot = empty_snapshot();
        let minimum_full_width = full_width(RESET_WIDTH, BAR_WIDTH_COMPACT);

        assert!(dashboard_layout(&snapshot, true, None).compact);
        assert!(dashboard_layout(&snapshot, false, Some(minimum_full_width as u16 - 1)).compact);
        assert!(!dashboard_layout(&snapshot, false, Some(minimum_full_width as u16)).compact);
        assert!(!dashboard_layout(&snapshot, false, None).compact);
    }

    #[test]
    fn layout_expands_bars_to_available_width() {
        let snapshot = empty_snapshot();
        let narrow = dashboard_layout(&snapshot, false, Some(70));
        let wide = dashboard_layout(&snapshot, false, Some(100));
        let compact = dashboard_layout(&snapshot, true, Some(70));

        assert!(wide.bar_width > narrow.bar_width);
        assert!(compact.bar_width > BAR_WIDTH_COMPACT);
        assert_eq!(full_width(RESET_WIDTH, wide.bar_width), 100);
        assert_eq!(compact_width(RESET_WIDTH, compact.bar_width), 70);
    }

    #[test]
    fn exit_keys_are_limited_to_q_and_control_shortcuts() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(is_exit_key(key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(is_exit_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(is_exit_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL)));
        assert!(!is_exit_key(key(KeyCode::Char('Q'), KeyModifiers::SHIFT)));
        assert!(!is_exit_key(key(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn resize_events_trigger_refresh() {
        assert!(is_resize_event(&Event::Resize(80, 24)));
        assert!(!is_resize_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))));
    }

    #[test]
    fn refresh_hint_counts_down_from_right_to_left() {
        let full = refresh_hint(30, 30, Some(100));
        let half = refresh_hint(30, 15, Some(100));
        let empty = refresh_hint(30, 0, Some(100));
        let dot_count = |hint: &str| hint.chars().filter(|character| *character == '.').count();

        assert_eq!(dot_count(&full), 30);
        assert_eq!(dot_count(&half), 15);
        assert_eq!(dot_count(&empty), 0);
        assert_eq!(full.chars().count(), half.chars().count());
        assert_eq!(half.chars().count(), empty.chars().count());
    }

    #[test]
    fn refresh_hint_scales_and_uses_determinate_progress_when_narrow() {
        assert_eq!(
            refresh_hint(60, 30, Some(100))
                .chars()
                .filter(|character| *character == '.')
                .count(),
            15
        );
        assert_eq!(refresh_hint(30, 12, None), " · refresh: 12s · q to exit");
        assert_eq!(refresh_hint(30, 12, Some(1)), " · ⠏");
        assert_eq!(refresh_progress(30, 30), '⣿');
        assert_eq!(refresh_progress(30, 15), '⠏');
        assert_eq!(refresh_progress(30, 0), '⠀');
    }

    #[test]
    fn narrow_title_keeps_indicator_visible_without_overflow() {
        let hint = refresh_hint(30, 12, Some(28));
        let title = dashboard_title_with_hint(false, Some(&hint), Some(28));
        let tiny_title = dashboard_title_with_hint(false, Some(&hint), Some(5));

        assert!(title.contains('⠏'));
        assert!(title.chars().count() <= 28);
        assert!(!title.contains("subscription quotas"));
        assert!(tiny_title.starts_with('⠏'));
        assert!(tiny_title.chars().count() <= 5);
    }

    #[test]
    fn usage_bar_clamps_at_one_hundred_percent() {
        assert_eq!(usage_bar(50, 4), "██░░");
        assert_eq!(usage_bar(100, 4), "████");
        assert_eq!(rounded_percent(100.1), 100);
    }

    #[test]
    fn colored_usage_bar_uses_regular_and_bright_terminal_colors() {
        let bar = colored_usage_bar(50, 4, true);
        assert!(bar.contains("██"));
        assert!(bar.contains("░░"));
        assert!(bar.contains(&SetForegroundColor(Color::DarkGreen).to_string()));
        assert!(bar.contains(&SetForegroundColor(Color::Green).to_string()));
        assert!(!bar.contains("\x1b[38;2;"));
        assert_eq!(usage_color(70), Color::DarkYellow);
        assert_eq!(unused_color(70), Color::Yellow);
        assert_eq!(usage_color(90), Color::DarkRed);
        assert_eq!(unused_color(90), Color::Red);
        assert_ne!(Color::DarkGrey, unused_color(50));
        assert_ne!(bar, usage_bar(50, 4));
    }

    #[test]
    fn reset_text_includes_days() {
        assert_eq!(reset_text(Some(90_061), 0), "1d 01h 01m");
    }

    #[test]
    fn parses_opencode_api_windows() {
        let data = serde_json::json!({
            "usage": {
                "rolling": { "status": "ok", "percent": 50.0, "resetsAt": "2026-01-01T03:00:00Z" },
                "weekly": { "status": "rate-limited", "percent": 75.0, "resetsAt": "2026-01-07T00:00:00Z" }
            }
        });
        let windows = opencode_api_windows(&data);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].status, UsageStatus::Ok);
        assert_eq!(windows[0].used_percent, Some(50.0));
        assert!(windows[0].reset_at.unwrap() > 1_000);
        assert_eq!(windows[0].window_seconds, Some(18_000));
        assert_eq!(windows[0].limit_reached, Some(false));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].status, UsageStatus::RateLimited);
        assert_eq!(windows[1].used_percent, Some(75.0));
        assert_eq!(windows[1].limit_reached, Some(true));
        assert_eq!(windows[1].window_seconds, Some(604_800));
    }

    #[test]
    fn json_contract_includes_version_statuses_and_numeric_fields() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![
                ProviderUsage {
                    provider: "opencode-go",
                    status: UsageStatus::RateLimited,
                    available: true,
                    plan: None,
                    source: Some("api_key"),
                    windows: vec![
                        UsageWindow {
                            label: "5h",
                            status: UsageStatus::RateLimited,
                            used_percent: Some(100.0),
                            reset_at: Some(2_000),
                            window_seconds: Some(18_000),
                            limit_reached: Some(true),
                        },
                        unavailable_window("7d"),
                    ],
                    error: None,
                    fetched_at: 1_000,
                },
                unavailable(Provider::Codex, "unavailable", 1_000),
            ],
        };

        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["fetched_at"], 1_000);
        assert_eq!(json["providers"][0]["status"], "rate_limited");
        assert_eq!(json["providers"][0]["windows"][0]["status"], "rate_limited");
        assert_eq!(json["providers"][0]["windows"][0]["used_percent"], 100.0);
        assert_eq!(json["providers"][0]["windows"][0]["reset_at"], 2_000);
        assert_eq!(json["providers"][0]["windows"][0]["window_seconds"], 18_000);
        assert_eq!(json["providers"][0]["windows"][1]["status"], "unavailable");
        assert!(json["providers"][0]["windows"][1]["used_percent"].is_null());
        assert_eq!(json["providers"][1]["status"], "unavailable");
    }

    fn sample_window(
        label: &'static str,
        status: UsageStatus,
        used_percent: f64,
        reset_at: u64,
    ) -> UsageWindow {
        UsageWindow {
            label,
            status,
            used_percent: Some(used_percent),
            reset_at: Some(reset_at),
            window_seconds: Some(18_000),
            limit_reached: Some(status == UsageStatus::RateLimited),
        }
    }

    fn sample_provider(
        provider: &'static str,
        status: UsageStatus,
        windows: Vec<UsageWindow>,
    ) -> ProviderUsage {
        ProviderUsage {
            provider,
            status,
            available: status != UsageStatus::Unavailable,
            plan: None,
            source: Some("oauth"),
            windows,
            error: None,
            fetched_at: 1_000,
        }
    }

    fn derived_json(providers: Vec<ProviderUsage>) -> serde_json::Value {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers,
        };
        serde_json::to_value(snapshot_view(&snapshot)).unwrap()
    }

    #[test]
    fn derived_display_keeps_raw_fields_and_adds_capacity() {
        let json = derived_json(vec![sample_provider(
            "codex",
            UsageStatus::Ok,
            vec![
                sample_window("5h", UsageStatus::Ok, 41.6, 2_000),
                sample_window("7d", UsageStatus::Ok, 10.0, 9_000),
            ],
        )]);
        let provider = &json["providers"][0];

        assert_eq!(json["schema_version"], 2);
        assert_eq!(provider["status"], "ok");
        assert_eq!(provider["windows"][0]["used_percent"], 41.6);
        assert_eq!(provider["display"]["name"], "Codex");
        assert_eq!(provider["display"]["exhausted"], false);
        assert_eq!(provider["display"]["capacity_used_percent"], 42);
        assert_eq!(provider["display"]["windows"][0]["label"], "5h");
        assert_eq!(provider["display"]["windows"][0]["used_percent"], 42);
        assert_eq!(provider["display"]["windows"][0]["reset_at"], 2_000);
        assert_eq!(provider["display"]["windows"][1]["label"], "7d");
        assert_eq!(provider["display"]["windows"][1]["used_percent"], 10);
        assert_eq!(provider["display"]["windows"][1]["reset_at"], 9_000);
        assert_eq!(provider["display"]["limiting_window"]["label"], "5h");
        assert_eq!(provider["display"]["limiting_window"]["used_percent"], 42);
        assert_eq!(provider["display"]["limiting_window"]["reset_at"], 2_000);
        assert_eq!(provider["display"]["next_reset_at"], 2_000);
        assert_eq!(json["best_available"]["provider"], "codex");
        assert_eq!(json["best_available"]["capacity_used_percent"], 42);
    }

    #[test]
    fn rate_limited_provider_is_exhausted_at_full_capacity() {
        let json = derived_json(vec![sample_provider(
            "opencode-go",
            UsageStatus::RateLimited,
            vec![
                sample_window("5h", UsageStatus::Ok, 96.0, 2_000),
                sample_window("7d", UsageStatus::RateLimited, 95.0, 9_000),
            ],
        )]);
        let display = &json["providers"][0]["display"];

        assert_eq!(display["exhausted"], true);
        assert_eq!(display["capacity_used_percent"], 100);
        assert_eq!(display["limiting_window"]["label"], "7d");
        assert_eq!(display["limiting_window"]["used_percent"], 95);
        assert_eq!(display["next_reset_at"], 2_000);
        assert_eq!(json["best_available"]["provider"], "opencode-go");
        assert_eq!(json["best_available"]["capacity_used_percent"], 100);
    }

    #[test]
    fn unavailable_provider_has_no_derived_capacity() {
        let json = derived_json(vec![unavailable(
            Provider::Codex,
            "credentials missing",
            1_000,
        )]);
        let display = &json["providers"][0]["display"];

        assert_eq!(json["providers"][0]["status"], "unavailable");
        assert_eq!(display["name"], "Codex");
        assert_eq!(display["exhausted"], false);
        assert!(display["capacity_used_percent"].is_null());
        assert!(display["windows"].is_null());
        assert!(display["limiting_window"].is_null());
        assert!(display["next_reset_at"].is_null());
        assert!(json["best_available"].is_null());
    }

    #[test]
    fn stale_provider_stays_usable_and_ignores_unavailable_windows() {
        let json = derived_json(vec![sample_provider(
            "claude-code",
            UsageStatus::Stale,
            vec![
                unavailable_window("5h"),
                sample_window("7d", UsageStatus::Ok, 55.4, 9_000),
            ],
        )]);
        let display = &json["providers"][0]["display"];

        assert_eq!(display["exhausted"], false);
        assert_eq!(display["capacity_used_percent"], 55);
        assert_eq!(display["limiting_window"]["label"], "7d");
        assert_eq!(display["next_reset_at"], 9_000);
        assert_eq!(json["best_available"]["provider"], "claude-code");
    }

    #[test]
    fn provider_without_usable_windows_is_not_comparable() {
        let json = derived_json(vec![
            sample_provider("codex", UsageStatus::Ok, vec![]),
            sample_provider(
                "opencode-go",
                UsageStatus::Ok,
                vec![unavailable_window("5h")],
            ),
        ]);

        assert!(json["providers"][0]["display"]["capacity_used_percent"].is_null());
        assert!(json["providers"][1]["display"]["capacity_used_percent"].is_null());
        assert!(json["best_available"].is_null());
    }

    #[test]
    fn best_available_picks_lowest_capacity_across_providers() {
        let json = derived_json(vec![
            sample_provider(
                "codex",
                UsageStatus::Ok,
                vec![sample_window("5h", UsageStatus::Ok, 80.0, 2_000)],
            ),
            sample_provider(
                "opencode-go",
                UsageStatus::RateLimited,
                vec![sample_window("5h", UsageStatus::RateLimited, 100.0, 3_000)],
            ),
            sample_provider(
                "claude-code",
                UsageStatus::Stale,
                vec![sample_window("5h", UsageStatus::Ok, 55.0, 4_000)],
            ),
            unavailable(Provider::Codex, "credentials missing", 1_000),
        ]);

        assert_eq!(json["providers"].as_array().unwrap().len(), 4);
        assert_eq!(json["best_available"]["provider"], "claude-code");
        assert_eq!(json["best_available"]["capacity_used_percent"], 55);
        assert_eq!(json["presentation"]["severity"], "ok");
    }

    #[test]
    fn severity_is_critical_only_when_no_provider_has_room() {
        let exhausted = |provider| {
            sample_provider(
                provider,
                UsageStatus::RateLimited,
                vec![sample_window("5h", UsageStatus::RateLimited, 100.0, 2_000)],
            )
        };
        let json = derived_json(vec![
            exhausted("codex"),
            exhausted("opencode-go"),
            unavailable(Provider::ClaudeCode, "credentials missing", 1_000),
        ]);

        assert_eq!(json["best_available"]["capacity_used_percent"], 100);
        assert_eq!(json["presentation"]["severity"], "critical");
        assert_eq!(
            json["presentation"]["summary"],
            "Codex 100% 5h ↻16m · Go 100% 5h ↻16m"
        );
    }

    #[test]
    fn ties_keep_response_and_snapshot_order() {
        let json = derived_json(vec![
            sample_provider(
                "codex",
                UsageStatus::Ok,
                vec![
                    sample_window("5h", UsageStatus::Ok, 42.0, 4_000),
                    sample_window("7d", UsageStatus::Ok, 42.0, 2_000),
                ],
            ),
            sample_provider(
                "opencode-go",
                UsageStatus::Ok,
                vec![sample_window("5h", UsageStatus::Ok, 42.0, 3_000)],
            ),
        ]);

        assert_eq!(
            json["providers"][0]["display"]["limiting_window"]["label"],
            "5h"
        );
        assert_eq!(json["providers"][0]["display"]["next_reset_at"], 2_000);
        assert_eq!(json["best_available"]["provider"], "codex");
        assert_eq!(json["best_available"]["capacity_used_percent"], 42);
    }

    #[test]
    fn presentation_summarizes_visible_providers_without_styling() {
        let json = derived_json(vec![
            sample_provider(
                "codex",
                UsageStatus::Ok,
                vec![
                    sample_window("5h", UsageStatus::Ok, 42.0, 1_000 + 7_200),
                    sample_window("7d", UsageStatus::Ok, 12.0, 1_000 + 259_200),
                ],
            ),
            sample_provider(
                "claude-code",
                UsageStatus::Stale,
                vec![sample_window("7d", UsageStatus::Ok, 35.0, 1_000 + 259_200)],
            ),
        ]);
        let presentation = &json["presentation"];

        assert_eq!(
            presentation["summary"],
            "Codex 42% 5h ↻2h · Claude 35% 7d ↻3d ≈"
        );
        assert_eq!(presentation["severity"], "ok");
        assert_eq!(presentation["providers"][0]["provider"], "codex");
        assert_eq!(
            presentation["providers"][0]["label"],
            "Codex       42% 5h ↻2h · 12% 7d ↻3d"
        );
        assert_eq!(presentation["providers"][0]["visible"], true);
        assert_eq!(
            presentation["providers"][1]["label"],
            "Claude Code 35% 7d ↻3d ≈"
        );
        assert_eq!(
            presentation["freshness"],
            freshness_text(&local_clock(1_000))
        );
        assert!(
            presentation["freshness"]
                .as_str()
                .is_some_and(|text| text.contains("↻ = reset in"))
        );
    }

    #[test]
    fn presentation_marks_providers_without_capacity_as_not_visible() {
        let json = derived_json(vec![
            unavailable(Provider::Codex, "credentials missing", 1_000),
            sample_provider("opencode-go", UsageStatus::Ok, vec![]),
        ]);
        let presentation = &json["presentation"];

        assert_eq!(presentation["summary"], "no usage data");
        assert_eq!(presentation["severity"], "unknown");
        assert_eq!(
            presentation["providers"][0]["label"],
            "Codex       unavailable"
        );
        assert_eq!(presentation["providers"][0]["visible"], false);
        assert_eq!(
            presentation["providers"][1]["label"],
            "OpenCode Go no usage data"
        );
        assert_eq!(presentation["providers"][1]["visible"], false);
    }

    #[test]
    fn severity_stays_semantic_and_tracks_dashboard_thresholds() {
        let best = |capacity_used_percent| {
            Some(BestAvailable {
                provider: "codex",
                capacity_used_percent,
            })
        };

        assert_eq!(severity(None), Severity::Unknown);
        assert_eq!(severity(best(0)), Severity::Ok);
        assert_eq!(severity(best(WARNING_PERCENT - 1)), Severity::Ok);
        assert_eq!(severity(best(WARNING_PERCENT)), Severity::Warning);
        assert_eq!(severity(best(CRITICAL_PERCENT - 1)), Severity::Warning);
        assert_eq!(severity(best(CRITICAL_PERCENT)), Severity::Critical);
        assert_eq!(severity(best(100)), Severity::Critical);
        assert_eq!(usage_color(WARNING_PERCENT), Color::DarkYellow);
        assert_eq!(usage_color(CRITICAL_PERCENT), Color::DarkRed);
    }

    #[test]
    fn compact_reset_keeps_the_largest_whole_unit() {
        assert_eq!(compact_reset(1_000 + 604_800, 1_000), "7d");
        assert_eq!(compact_reset(1_000 + 10_800, 1_000), "3h");
        assert_eq!(compact_reset(1_000 + 720, 1_000), "12m");
        assert_eq!(compact_reset(1_000 + 30, 1_000), "30s");
        assert_eq!(compact_reset(1_000, 2_000), "now");
        assert_eq!(
            freshness_text("14:03:22"),
            "Updated 14:03:22 · ↻ = reset in · ≈ = stale"
        );
        let clock = local_clock(1_783_871_200);
        assert_eq!(clock.chars().count(), 8);
        assert!(
            clock.chars().enumerate().all(|(index, character)| {
                if index == 2 || index == 5 {
                    character == ':'
                } else {
                    character.is_ascii_digit()
                }
            }),
            "unexpected clock format: {clock}"
        );
        assert_eq!(local_clock(u64::MAX), "unknown");
    }

    #[test]
    fn missing_codex_primary_window_remains_visible() {
        let data = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 21.0,
                    "reset_at": 2_000,
                    "limit_window_seconds": 604_800
                }
            }
        });

        let windows = codex_windows(&data);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].used_percent, None);
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].used_percent, Some(21.0));
    }

    #[test]
    fn dashboard_labels_usage_and_right_aligns_resets() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                status: UsageStatus::Ok,
                available: true,
                plan: Some("Plus".to_owned()),
                source: None,
                windows: vec![
                    UsageWindow {
                        label: "5h",
                        status: UsageStatus::Ok,
                        used_percent: Some(50.0),
                        reset_at: Some(4_600),
                        window_seconds: Some(18_000),
                        limit_reached: None,
                    },
                    UsageWindow {
                        label: "7d",
                        status: UsageStatus::Ok,
                        used_percent: Some(50.0),
                        reset_at: Some(91_061),
                        window_seconds: Some(604_800),
                        limit_reached: None,
                    },
                    unavailable_window("month"),
                ],
                error: None,
                fetched_at: 1_000,
            }],
        };

        let dashboard = render_dashboard(&snapshot, false, default_layout(false));
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], dashboard_title(false));
        assert!(lines[1].ends_with("resets in"));
        assert_eq!(
            lines[2],
            provider_separator(false, RESET_WIDTH, false, BAR_WIDTH)
        );
        assert!(
            lines[3..]
                .iter()
                .all(|line| line.chars().count() == lines[1].chars().count())
        );
        assert!(lines[3].starts_with(" Codex           5h "));
        assert!(lines[3].ends_with("1h 00m"));
        assert!(lines[4].ends_with("1d 01h 01m"));
        assert!(lines[5].ends_with("unavailable"));
        assert!(!dashboard.contains("Plus"));
        assert_eq!(dashboard.matches("resets in").count(), 1);
        assert!(!dashboard.contains("reset in"));
    }

    #[test]
    fn dashboard_mutes_unavailable_windows() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                status: UsageStatus::Ok,
                available: true,
                plan: None,
                source: None,
                windows: vec![unavailable_window("5h")],
                error: None,
                fetched_at: 1_000,
            }],
        };

        assert_eq!(
            render_dashboard(&snapshot, false, default_layout(false)),
            format!(
                "{}\n Codex           5h [{}]       unavailable",
                dashboard_title(false),
                usage_bar(0, BAR_WIDTH)
            )
        );
        let dashboard = render_dashboard(&snapshot, true, default_layout(false));
        assert!(dashboard.contains(&SetForegroundColor(Color::DarkGrey).to_string()));
        assert!(!dashboard.contains("\x1b[38;2;"));
    }

    #[test]
    fn dashboard_separates_providers() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![
                ProviderUsage {
                    provider: "codex",
                    status: UsageStatus::Ok,
                    available: true,
                    plan: None,
                    source: None,
                    windows: vec![UsageWindow {
                        label: "5h",
                        status: UsageStatus::Ok,
                        used_percent: Some(50.0),
                        reset_at: Some(4_600),
                        window_seconds: Some(18_000),
                        limit_reached: None,
                    }],
                    error: None,
                    fetched_at: 1_000,
                },
                unavailable(Provider::OpencodeGo, "credentials missing", 1_000),
            ],
        };

        let dashboard = render_dashboard(&snapshot, false, default_layout(false));
        assert!(
            dashboard
                .lines()
                .any(|line| line == provider_separator(false, RESET_WIDTH, false, BAR_WIDTH))
        );
    }

    #[test]
    fn dashboard_includes_unavailable_providers() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![unavailable(
                Provider::Codex,
                "Codex OAuth credential not found",
                1_000,
            )],
        };
        assert!(
            render_dashboard(&snapshot, false, default_layout(false))
                .contains("unavailable: Codex OAuth credential not found")
        );
        assert_eq!(exit_code(&snapshot), 1);
    }

    #[test]
    fn compact_layout_stacks_provider_and_uses_narrow_bars() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                status: UsageStatus::Ok,
                available: true,
                plan: None,
                source: None,
                windows: vec![UsageWindow {
                    label: "5h",
                    status: UsageStatus::Ok,
                    used_percent: Some(50.0),
                    reset_at: Some(4_600),
                    window_seconds: Some(18_000),
                    limit_reached: None,
                }],
                error: None,
                fetched_at: 1_000,
            }],
        };

        let dashboard = render_dashboard(&snapshot, false, default_layout(true));
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], dashboard_title(false));
        assert_eq!(lines[1], " Codex");
        assert!(lines[2].starts_with("    5h ["));
        assert!(lines[2].contains(&usage_bar(50, BAR_WIDTH_COMPACT)));
        assert!(lines[2].chars().count() < 60);
    }

    #[test]
    fn compact_layout_separates_providers() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![
                ProviderUsage {
                    provider: "codex",
                    status: UsageStatus::Ok,
                    available: true,
                    plan: None,
                    source: None,
                    windows: vec![UsageWindow {
                        label: "5h",
                        status: UsageStatus::Ok,
                        used_percent: Some(50.0),
                        reset_at: Some(4_600),
                        window_seconds: Some(18_000),
                        limit_reached: None,
                    }],
                    error: None,
                    fetched_at: 1_000,
                },
                unavailable(Provider::OpencodeGo, "credentials missing", 1_000),
            ],
        };

        let dashboard = render_dashboard(&snapshot, false, default_layout(true));
        let separator = provider_separator(false, RESET_WIDTH, true, BAR_WIDTH_COMPACT);
        assert!(dashboard.contains(&separator));
    }

    #[test]
    fn compact_layout_shows_unavailable_provider() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![unavailable(
                Provider::Codex,
                "Codex OAuth credential not found",
                1_000,
            )],
        };
        let dashboard = render_dashboard(&snapshot, false, default_layout(true));
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], dashboard_title(false));
        assert_eq!(lines[1], " Codex");
        assert!(lines[2].contains("unavailable: Codex OAuth credential not found"));
    }

    #[test]
    fn compact_layout_mutes_unavailable_window() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                status: UsageStatus::Ok,
                available: true,
                plan: None,
                source: None,
                windows: vec![unavailable_window("5h")],
                error: None,
                fetched_at: 1_000,
            }],
        };

        let dashboard = render_dashboard(&snapshot, false, default_layout(true));
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], dashboard_title(false));
        assert_eq!(lines[1], " Codex");
        assert!(lines[2].contains(&usage_bar(0, BAR_WIDTH_COMPACT)));
        assert!(lines[2].contains("unavailable"));
    }

    #[test]
    fn compact_layout_with_multiple_windows() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                status: UsageStatus::Ok,
                available: true,
                plan: None,
                source: None,
                windows: vec![
                    UsageWindow {
                        label: "5h",
                        status: UsageStatus::Ok,
                        used_percent: Some(50.0),
                        reset_at: Some(4_600),
                        window_seconds: Some(18_000),
                        limit_reached: None,
                    },
                    UsageWindow {
                        label: "7d",
                        status: UsageStatus::Ok,
                        used_percent: Some(25.0),
                        reset_at: Some(10_000),
                        window_seconds: Some(604_800),
                        limit_reached: None,
                    },
                    UsageWindow {
                        label: "30d",
                        status: UsageStatus::Ok,
                        used_percent: Some(10.0),
                        reset_at: Some(100_000),
                        window_seconds: Some(2_592_000),
                        limit_reached: None,
                    },
                ],
                error: None,
                fetched_at: 1_000,
            }],
        };

        let dashboard = render_dashboard(&snapshot, false, default_layout(true));
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], dashboard_title(false));
        assert_eq!(lines[1], " Codex");
        assert!(lines[2].starts_with("    5h ["));
        assert!(lines[3].starts_with("    7d ["));
        assert!(lines[4].starts_with("   30d ["));
        assert_eq!(lines[2].find('['), lines[3].find('['));
        assert_eq!(lines[3].find('['), lines[4].find('['));
    }

    #[test]
    fn compact_layout_with_colors() {
        let snapshot = Snapshot {
            schema_version: JSON_SCHEMA_VERSION,
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                status: UsageStatus::Ok,
                available: true,
                plan: None,
                source: None,
                windows: vec![unavailable_window("5h")],
                error: None,
                fetched_at: 1_000,
            }],
        };

        assert!(
            render_dashboard(&snapshot, true, default_layout(true))
                .contains(&SetForegroundColor(Color::DarkGrey).to_string())
        );
    }
}
