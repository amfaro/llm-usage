use std::{
    env,
    fs::File,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use regex::Regex;
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::Serialize;
use serde_json::Value;

const BAR_WIDTH: usize = 28;
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
    used_percent: f64,
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
    let colors =
        io::stdout().is_terminal() && !args.display.no_color && env::var_os("NO_COLOR").is_none();
    loop {
        let snapshot = fetch_snapshot(&args.display.query.providers);
        if io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H");
        }
        println!(
            "{}\n\nrefresh: {}s · Ctrl+C to exit",
            render_dashboard(&snapshot, colors),
            args.interval
        );
        let _ = io::stdout().flush();
        thread::sleep(Duration::from_secs(args.interval));
    }
}

fn print_once(args: &DisplayArgs) -> i32 {
    let snapshot = fetch_snapshot(&args.query.providers);
    let colors = io::stdout().is_terminal() && !args.no_color && env::var_os("NO_COLOR").is_none();
    println!("{}", render_dashboard(&snapshot, colors));
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
    let windows = [
        codex_window(
            data.pointer("/rate_limit/primary_window")
                .or_else(|| data.get("primary_window")),
            "5h",
            &data,
        ),
        codex_window(
            data.pointer("/rate_limit/secondary_window")
                .or_else(|| data.get("secondary_window")),
            "7d",
            &data,
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
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

fn codex_window(raw: Option<&Value>, fallback: &'static str, data: &Value) -> Option<UsageWindow> {
    let raw = raw?;
    let used_percent = raw.get("used_percent")?.as_f64()?.clamp(0.0, 100.0);
    let window_seconds = raw.get("limit_window_seconds").and_then(Value::as_u64);
    Some(UsageWindow {
        label: window_label(window_seconds, fallback),
        used_percent,
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
            used_percent,
            reset_at: Some(fetched_at.saturating_add(reset_in_seconds)),
            window_seconds: Some(seconds),
            limit_reached: None,
        })
    })
}

fn render_dashboard(snapshot: &Snapshot, colors: bool) -> String {
    let mut lines = vec!["Usage dashboard".to_owned()];
    for provider in &snapshot.providers {
        let label = match provider.provider {
            "codex" => Provider::Codex.label(),
            "opencode-go" => Provider::OpencodeGo.label(),
            _ => provider.provider,
        };
        let heading = match &provider.plan {
            Some(plan) => format!("{label} · {plan}"),
            None => label.to_owned(),
        };
        lines.push(heading);
        if !provider.available {
            lines.push(format!(
                "  unavailable: {}",
                provider.error.unwrap_or("usage unavailable")
            ));
            continue;
        }
        for window in &provider.windows {
            let percent = rounded_percent(window.used_percent);
            let bar = usage_bar(percent, BAR_WIDTH);
            let bar = if colors {
                format!("{}{}\x1b[0m", usage_color(percent), bar)
            } else {
                bar
            };
            lines.push(format!(
                "  {:<3} [{}] {:>3}%{}",
                window.label,
                bar,
                percent,
                reset_text(window.reset_at, snapshot.fetched_at)
            ));
        }
    }
    lines.join("\n")
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
        return " → resets now".to_owned();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!(" → reset in {hours}h {minutes}m")
    } else {
        format!(" → reset in {minutes}m")
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
    fn usage_bar_clamps_at_one_hundred_percent() {
        assert_eq!(usage_bar(50, 4), "██░░");
        assert_eq!(usage_bar(100, 4), "████");
        assert_eq!(rounded_percent(100.1), 100);
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
        assert_eq!(windows[1].used_percent, 25.0);
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
            render_dashboard(&snapshot, false)
                .contains("unavailable: Codex OAuth credential not found")
        );
        assert_eq!(exit_code(&snapshot), 1);
    }
}
