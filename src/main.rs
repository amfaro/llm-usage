use std::{
    env,
    fs::File,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use regex::Regex;
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::Serialize;
use serde_json::Value;

const BAR_WIDTH: usize = 28;
const BAR_WIDTH_COMPACT: usize = 16;
const PROVIDER_WIDTH: usize = 11;
const WINDOW_WIDTH: usize = 6;
const COMPACT_WINDOW_WIDTH: usize = 3;
const RESET_WIDTH: usize = "unavailable".len();
const COMPACT_THRESHOLD: u16 = 80;
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const OPENCODE_GO_URL: &str = "https://opencode.ai";

#[derive(Parser)]
#[command(about = "Show Codex and OpenCode Go subscription quotas")]
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
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpencodeGo => "opencode-go",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::OpencodeGo => "OpenCode Go",
        }
    }
}

#[derive(Serialize)]
struct Snapshot {
    fetched_at: u64,
    providers: Vec<ProviderUsage>,
}

#[derive(Serialize)]
struct ProviderUsage {
    provider: &'static str,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    windows: Vec<UsageWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
    fetched_at: u64,
}

#[derive(Serialize)]
struct UsageWindow {
    label: &'static str,
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
        let compact = compact_layout(
            args.display.compact,
            terminal
                .then(size)
                .and_then(Result::ok)
                .map(|(cols, _)| cols),
        );
        let snapshot = fetch_snapshot(&args.display.query.providers);
        if terminal {
            print!("\x1b[2J\x1b[H");
        }
        println!(
            "{}\n\nrefresh: {}s · q to exit",
            render_dashboard(&snapshot, colors, compact),
            args.interval
        );
        let _ = io::stdout().flush();
        let raw_mode = io::stdin()
            .is_terminal()
            .then(RawMode::enable)
            .transpose()
            .ok()
            .flatten();
        if wait_for_exit(Duration::from_secs(args.interval), raw_mode.is_some()) {
            return 0;
        }
    }
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

fn wait_for_exit(timeout: Duration, read_keys: bool) -> bool {
    if !read_keys {
        thread::sleep(timeout);
        return false;
    }

    let deadline = Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        if !event::poll(remaining).unwrap_or(false) {
            return false;
        }
        if matches!(event::read(), Ok(Event::Key(key)) if is_exit_key(key)) {
            return true;
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

fn print_once(args: &DisplayArgs) -> i32 {
    let snapshot = fetch_snapshot(&args.query.providers);
    let terminal = io::stdout().is_terminal();
    let colors = terminal && !args.no_color && env::var_os("NO_COLOR").is_none();
    let compact = compact_layout(
        args.compact,
        terminal
            .then(size)
            .and_then(Result::ok)
            .map(|(cols, _)| cols),
    );
    println!("{}", render_dashboard(&snapshot, colors, compact));
    exit_code(&snapshot)
}

fn print_json(args: &QueryArgs) -> i32 {
    let snapshot = fetch_snapshot(&args.providers);
    println!(
        "{}",
        serde_json::to_string_pretty(&snapshot).expect("snapshot serializes")
    );
    exit_code(&snapshot)
}

fn compact_layout(force: bool, columns: Option<u16>) -> bool {
    force || columns.is_some_and(|columns| columns <= COMPACT_THRESHOLD)
}

fn exit_code(snapshot: &Snapshot) -> i32 {
    i32::from(!snapshot.providers.iter().any(|provider| provider.available))
}

fn fetch_snapshot(requested: &[Provider]) -> Snapshot {
    let providers = if requested.is_empty() {
        vec![Provider::Codex, Provider::OpencodeGo]
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
        fetched_at,
        providers: providers
            .into_iter()
            .map(|provider| match provider {
                Provider::Codex => fetch_codex(&client, fetched_at),
                Provider::OpencodeGo => fetch_opencode_go(fetched_at),
            })
            .collect(),
    }
}

fn unavailable(provider: Provider, error: &'static str, fetched_at: u64) -> ProviderUsage {
    ProviderUsage {
        provider: provider.name(),
        available: false,
        plan: None,
        source: None,
        windows: vec![],
        error: Some(error),
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
    Some(UsageWindow {
        label: window_label(window_seconds, fallback),
        used_percent: Some(used_percent),
        reset_at: raw.get("reset_at").and_then(Value::as_u64),
        window_seconds,
        limit_reached: data
            .get("limit_reached")
            .and_then(Value::as_bool)
            .or_else(|| {
                data.pointer("/rate_limit/limit_reached")
                    .and_then(Value::as_bool)
            })
            .or_else(|| raw.get("limit_reached").and_then(Value::as_bool)),
    })
}

fn unavailable_window(label: &'static str) -> UsageWindow {
    UsageWindow {
        label,
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

fn fetch_opencode_go(fetched_at: u64) -> ProviderUsage {
    let (Some(workspace_id), Some(auth_cookie)) = (
        nonempty_env("OPENCODE_GO_WORKSPACE_ID"),
        nonempty_env("OPENCODE_GO_AUTH_COOKIE"),
    ) else {
        return unavailable(
            Provider::OpencodeGo,
            "set OPENCODE_GO_WORKSPACE_ID and OPENCODE_GO_AUTH_COOKIE",
            fetched_at,
        );
    };
    let mut url = Url::parse(OPENCODE_GO_URL).expect("constant URL parses");
    url.path_segments_mut()
        .expect("base URL supports paths")
        .extend(["workspace", workspace_id.as_str(), "go"]);
    let cookie = if auth_cookie.trim_start().starts_with("auth=") {
        auth_cookie
    } else {
        format!("auth={auth_cookie}")
    };
    let Ok(client) = Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(Policy::none())
        .user_agent(concat!("llm-usage/", env!("CARGO_PKG_VERSION")))
        .build()
    else {
        return unavailable(
            Provider::OpencodeGo,
            "OpenCode HTTP client failed",
            fetched_at,
        );
    };
    let response = match client.get(url).header("Cookie", cookie).send() {
        Ok(response) if response.status().is_success() => response,
        Ok(response)
            if response.status().is_redirection()
                || response.status().as_u16() == 401
                || response.status().as_u16() == 403 =>
        {
            return unavailable(
                Provider::OpencodeGo,
                "OpenCode dashboard authentication failed",
                fetched_at,
            );
        }
        Ok(_) | Err(_) => {
            return unavailable(
                Provider::OpencodeGo,
                "OpenCode dashboard request failed",
                fetched_at,
            );
        }
    };
    let Ok(html) = response.text() else {
        return unavailable(
            Provider::OpencodeGo,
            "OpenCode dashboard response was invalid",
            fetched_at,
        );
    };
    let windows = opencode_windows(&html, fetched_at);
    if windows.is_empty() {
        return unavailable(
            Provider::OpencodeGo,
            "OpenCode dashboard usage was not found",
            fetched_at,
        );
    }
    ProviderUsage {
        provider: Provider::OpencodeGo.name(),
        available: true,
        plan: Some("Go".to_owned()),
        source: Some("dashboard"),
        windows,
        error: None,
        fetched_at,
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn opencode_windows(html: &str, fetched_at: u64) -> Vec<UsageWindow> {
    [
        (&["rolling5h", "rolling", "rollingUsage"][..], "5h", 18_000),
        (&["weekly", "weeklyUsage"][..], "7d", 604_800),
        (&["monthly", "monthlyUsage"][..], "30d", 2_592_000),
    ]
    .into_iter()
    .filter_map(|(keys, label, seconds)| opencode_window(html, keys, label, seconds, fetched_at))
    .collect()
}

fn opencode_window(
    html: &str,
    keys: &[&str],
    label: &'static str,
    seconds: u64,
    fetched_at: u64,
) -> Option<UsageWindow> {
    let key_pattern = keys.join("|");
    let object = Regex::new(&format!(r#"(?s)["']?(?:{key_pattern})["']?\s*[:=]\s*(?:\$R\[\d+\]\s*=\s*)?\{{(?P<object>[^{{}}]*)\}}"#)).ok()?;
    let percent = Regex::new(r#"(?i)["']?(?:usagePercent|usedPercent|percent)["']?\s*[:=]\s*["']?([0-9]+(?:\.[0-9]+)?)["']?"#).expect("constant regex parses");
    let reset = Regex::new(r#"(?i)["']?(?:resetInSec|resetAfterSeconds|resetAfterSec)["']?\s*[:=]\s*["']?([0-9]+(?:\.[0-9]+)?)["']?"#).expect("constant regex parses");
    object.captures_iter(html).find_map(|captures| {
        let object = captures.name("object")?.as_str();
        let used_percent = percent
            .captures(object)?
            .get(1)?
            .as_str()
            .parse::<f64>()
            .ok()?
            .clamp(0.0, 100.0);
        let reset_in_seconds = reset
            .captures(object)?
            .get(1)?
            .as_str()
            .parse::<u64>()
            .ok()?;
        Some(UsageWindow {
            label,
            used_percent: Some(used_percent),
            reset_at: Some(fetched_at.saturating_add(reset_in_seconds)),
            window_seconds: Some(seconds),
            limit_reached: None,
        })
    })
}

#[allow(clippy::too_many_lines)]
fn render_dashboard(snapshot: &Snapshot, colors: bool, compact: bool) -> String {
    let reset_width = snapshot
        .providers
        .iter()
        .flat_map(|provider| provider.windows.iter())
        .filter_map(|window| {
            window
                .reset_at
                .map(|reset_at| reset_text(Some(reset_at), snapshot.fetched_at))
        })
        .map(|text| text.chars().count())
        .fold(RESET_WIDTH, usize::max);
    let bar_width = if compact {
        BAR_WIDTH_COMPACT
    } else {
        BAR_WIDTH
    };
    let mut lines = vec![];
    let mut rendered_header = false;
    for (index, provider) in snapshot.providers.iter().enumerate() {
        if index > 0 {
            lines.push(provider_separator(colors, reset_width, compact));
        }
        let label = match provider.provider {
            "codex" => Provider::Codex.label(),
            "opencode-go" => Provider::OpencodeGo.label(),
            _ => provider.provider,
        };
        if !provider.available {
            if compact {
                lines.push(format!(" {label}"));
                lines.push(format!(
                    "   unavailable: {}",
                    provider.error.unwrap_or("usage unavailable")
                ));
            } else {
                lines.push(format!(
                    " {:<PROVIDER_WIDTH$} unavailable: {}",
                    label,
                    provider.error.unwrap_or("usage unavailable")
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
            lines.push(header_text(colors, reset_width));
            lines.push(provider_separator(colors, reset_width, false));
            rendered_header = true;
        }
        if compact {
            lines.push(format!(" {label}"));
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
            let bar = usage_bar(percent, bar_width);
            let bar = if colors {
                format!("{}{}\x1b[0m", usage_color(percent), bar)
            } else {
                bar
            };
            if compact {
                lines.push(format!(
                    "   {:>COMPACT_WINDOW_WIDTH$} [{}] {:>3}%  {:>reset_width$}",
                    window.label,
                    bar,
                    percent,
                    reset_text(window.reset_at, snapshot.fetched_at)
                ));
            } else {
                lines.push(format!(
                    " {:<PROVIDER_WIDTH$} {:>WINDOW_WIDTH$} [{}] {:>3}%  {:>reset_width$}",
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

fn header_text(colors: bool, reset_width: usize) -> String {
    let header = format!(
        " {:<PROVIDER_WIDTH$} {:>WINDOW_WIDTH$} {:<35}  {:>reset_width$}",
        "provider", "meter", "usage", "resets in"
    );
    if colors { muted_text(&header) } else { header }
}

fn provider_separator(colors: bool, reset_width: usize, compact: bool) -> String {
    let width = if compact {
        3 + COMPACT_WINDOW_WIDTH + 1 + 2 + BAR_WIDTH_COMPACT + 1 + 4 + 2 + reset_width
    } else {
        header_text(false, reset_width).chars().count()
    };
    let separator = "─".repeat(width);
    if colors {
        muted_text(&separator)
    } else {
        separator
    }
}

fn muted_text(text: &str) -> String {
    format!("\x1b[38;2;156;163;175m{text}\x1b[0m")
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

fn usage_color(percent: u8) -> &'static str {
    if percent >= 90 {
        "\x1b[38;2;239;68;68m"
    } else if percent >= 70 {
        "\x1b[38;2;250;204;21m"
    } else {
        "\x1b[38;2;34;197;94m"
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

    #[test]
    fn compact_layout_uses_flag_or_narrow_terminal() {
        assert!(compact_layout(true, None));
        assert!(compact_layout(false, Some(COMPACT_THRESHOLD)));
        assert!(compact_layout(false, Some(COMPACT_THRESHOLD - 1)));
        assert!(!compact_layout(false, Some(COMPACT_THRESHOLD + 1)));
        assert!(!compact_layout(false, None));
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
    fn usage_bar_clamps_at_one_hundred_percent() {
        assert_eq!(usage_bar(50, 4), "██░░");
        assert_eq!(usage_bar(100, 4), "████");
        assert_eq!(rounded_percent(100.1), 100);
    }

    #[test]
    fn reset_text_includes_days() {
        assert_eq!(reset_text(Some(90_061), 0), "1d 01h 01m");
    }

    #[test]
    fn parses_opencode_dashboard_windows_in_either_field_order() {
        let html = r#"
            <script>let a={rolling5h:{usagePercent:50,resetInSec:7200}};</script>
            <script>let b={weekly:{resetInSec:3600,usagePercent:25}};</script>
        "#;
        let windows = opencode_windows(html, 1_000);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].reset_at, Some(8_200));
        assert_eq!(windows[1].label, "7d");
        assert_eq!(windows[1].used_percent, Some(25.0));
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
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                available: true,
                plan: Some("Plus".to_owned()),
                source: None,
                windows: vec![
                    UsageWindow {
                        label: "5h",
                        used_percent: Some(50.0),
                        reset_at: Some(4_600),
                        window_seconds: Some(18_000),
                        limit_reached: None,
                    },
                    UsageWindow {
                        label: "7d",
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

        let dashboard = render_dashboard(&snapshot, false, false);
        let lines: Vec<_> = dashboard.lines().collect();
        assert!(lines[0].ends_with("resets in"));
        assert_eq!(lines[1], provider_separator(false, RESET_WIDTH, false));
        assert!(
            lines[2..]
                .iter()
                .all(|line| line.chars().count() == lines[0].chars().count())
        );
        assert!(lines[2].starts_with(" Codex           5h "));
        assert!(lines[2].ends_with("1h 00m"));
        assert!(lines[3].ends_with("1d 01h 01m"));
        assert!(lines[4].ends_with("unavailable"));
        assert!(!dashboard.contains("Plus"));
        assert_eq!(dashboard.matches("resets in").count(), 1);
        assert!(!dashboard.contains("reset in"));
    }

    #[test]
    fn dashboard_mutes_unavailable_windows() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                available: true,
                plan: None,
                source: None,
                windows: vec![unavailable_window("5h")],
                error: None,
                fetched_at: 1_000,
            }],
        };

        assert_eq!(
            render_dashboard(&snapshot, false, false),
            format!(
                " Codex           5h [{}]       unavailable",
                usage_bar(0, BAR_WIDTH)
            )
        );
        assert!(render_dashboard(&snapshot, true, false).contains("\x1b[38;2;156;163;175m"));
    }

    #[test]
    fn dashboard_separates_providers() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![
                ProviderUsage {
                    provider: "codex",
                    available: true,
                    plan: None,
                    source: None,
                    windows: vec![UsageWindow {
                        label: "5h",
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

        let dashboard = render_dashboard(&snapshot, false, false);
        assert!(
            dashboard
                .lines()
                .any(|line| line == provider_separator(false, RESET_WIDTH, false))
        );
    }

    #[test]
    fn dashboard_includes_unavailable_providers() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![unavailable(
                Provider::Codex,
                "Codex OAuth credential not found",
                1_000,
            )],
        };
        assert!(
            render_dashboard(&snapshot, false, false)
                .contains("unavailable: Codex OAuth credential not found")
        );
        assert_eq!(exit_code(&snapshot), 1);
    }

    #[test]
    fn compact_layout_stacks_provider_and_uses_narrow_bars() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                available: true,
                plan: None,
                source: None,
                windows: vec![UsageWindow {
                    label: "5h",
                    used_percent: Some(50.0),
                    reset_at: Some(4_600),
                    window_seconds: Some(18_000),
                    limit_reached: None,
                }],
                error: None,
                fetched_at: 1_000,
            }],
        };

        let dashboard = render_dashboard(&snapshot, false, true);
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], " Codex");
        assert!(lines[1].starts_with("    5h ["));
        assert!(lines[1].contains(&usage_bar(50, BAR_WIDTH_COMPACT)));
        assert!(lines[1].chars().count() < 60);
    }

    #[test]
    fn compact_layout_separates_providers() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![
                ProviderUsage {
                    provider: "codex",
                    available: true,
                    plan: None,
                    source: None,
                    windows: vec![UsageWindow {
                        label: "5h",
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

        let dashboard = render_dashboard(&snapshot, false, true);
        let separator = provider_separator(false, RESET_WIDTH, true);
        assert!(dashboard.contains(&separator));
    }

    #[test]
    fn compact_layout_shows_unavailable_provider() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![unavailable(
                Provider::Codex,
                "Codex OAuth credential not found",
                1_000,
            )],
        };
        let dashboard = render_dashboard(&snapshot, false, true);
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], " Codex");
        assert!(lines[1].contains("unavailable: Codex OAuth credential not found"));
    }

    #[test]
    fn compact_layout_mutes_unavailable_window() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                available: true,
                plan: None,
                source: None,
                windows: vec![unavailable_window("5h")],
                error: None,
                fetched_at: 1_000,
            }],
        };

        let dashboard = render_dashboard(&snapshot, false, true);
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], " Codex");
        assert!(lines[1].contains(&usage_bar(0, BAR_WIDTH_COMPACT)));
        assert!(lines[1].contains("unavailable"));
    }

    #[test]
    fn compact_layout_with_multiple_windows() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                available: true,
                plan: None,
                source: None,
                windows: vec![
                    UsageWindow {
                        label: "5h",
                        used_percent: Some(50.0),
                        reset_at: Some(4_600),
                        window_seconds: Some(18_000),
                        limit_reached: None,
                    },
                    UsageWindow {
                        label: "7d",
                        used_percent: Some(25.0),
                        reset_at: Some(10_000),
                        window_seconds: Some(604_800),
                        limit_reached: None,
                    },
                    UsageWindow {
                        label: "30d",
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

        let dashboard = render_dashboard(&snapshot, false, true);
        let lines: Vec<_> = dashboard.lines().collect();
        assert_eq!(lines[0], " Codex");
        assert!(lines[1].starts_with("    5h ["));
        assert!(lines[2].starts_with("    7d ["));
        assert!(lines[3].starts_with("   30d ["));
        assert_eq!(lines[1].find('['), lines[2].find('['));
        assert_eq!(lines[2].find('['), lines[3].find('['));
    }

    #[test]
    fn compact_layout_with_colors() {
        let snapshot = Snapshot {
            fetched_at: 1_000,
            providers: vec![ProviderUsage {
                provider: "codex",
                available: true,
                plan: None,
                source: None,
                windows: vec![unavailable_window("5h")],
                error: None,
                fetched_at: 1_000,
            }],
        };

        assert!(render_dashboard(&snapshot, true, true).contains("\x1b[38;2;156;163;175m"));
    }
}
