//! The only door to AWS. Every API call goes through `Aws::call`, which refuses
//! anything that is not a read on an allowlisted service.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Ctx {
    pub profile: String,
    pub region: String,
}

#[derive(Clone, Debug)]
pub enum AwsError {
    /// SSO token missing or expired for this profile.
    SsoExpired(String),
    /// Refused by the read-only guard, or an API / CLI error.
    Api(String),
}

impl std::fmt::Display for AwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AwsError::SsoExpired(p) if p.is_empty() => write!(f, "SSO session expired or no credentials: run `aws sso login`, then press r"),
            AwsError::SsoExpired(p) => write!(f, "SSO session expired: press L to log in (aws sso login --profile {p})"),
            AwsError::Api(m) => f.write_str(m),
        }
    }
}

const SERVICES: &[&str] = &[
    "ecs", "logs", "elbv2", "application-autoscaling", "ec2", "ecr", "sqs", "rds", "lambda", "cloudwatch", "sts",
];
const READ_PREFIXES: &[&str] = &["describe-", "list-", "get-"];
const READ_EXACT: &[&str] = &["filter-log-events"];
// Reads that hand out credentials. Not mutations, but nothing here needs them.
const DENIED: &[&str] = &["get-login-password", "get-authorization-token", "get-session-token", "get-federation-token"];

pub fn check_read_only(service: &str, op: &str) -> Result<(), AwsError> {
    let ok = SERVICES.contains(&service)
        && !DENIED.contains(&op)
        && (READ_EXACT.contains(&op) || READ_PREFIXES.iter().any(|p| op.starts_with(p)));
    if ok { Ok(()) } else { Err(AwsError::Api(format!("refused by read-only guard: aws {service} {op}"))) }
}

type Runner = std::sync::Arc<dyn Fn(&Ctx, &str, &str, &[String]) -> Result<Value, AwsError> + Send + Sync>;

pub struct Aws {
    pub ctx: Ctx,
    runner: Runner,
    /// `--readonly`: refuse even the scale writes.
    pub readonly: bool,
    /// The screen whose command log records these calls (0: none).
    pub screen: u64,
}

/// The only writes the tool can make: scaling. Arguments are built here from typed values,
/// so nothing else (task definition, network, deployment config) can ride along.
#[derive(Clone, Debug, PartialEq)]
pub enum Scale {
    /// `ecs update-service --desired-count`.
    Desired { cluster: String, service: String, count: i64 },
    /// `application-autoscaling register-scalable-target` with min and max capacity.
    Capacity { ns: String, resource: String, dimension: String, min: i64, max: i64 },
}

impl Scale {
    pub fn command(&self) -> (&'static str, &'static str, Vec<String>) {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Scale::Desired { cluster, service, count } => {
                ("ecs", "update-service", v(&["--cluster", cluster, "--service", service, "--desired-count", &count.to_string()]))
            }
            Scale::Capacity { ns, resource, dimension, min, max } => (
                "application-autoscaling",
                "register-scalable-target",
                v(&["--service-namespace", ns, "--resource-id", resource, "--scalable-dimension", dimension,
                    "--min-capacity", &min.to_string(), "--max-capacity", &max.to_string()]),
            ),
        }
    }
}

impl Aws {
    pub fn new(ctx: Ctx) -> Self {
        Aws { ctx, runner: std::sync::Arc::new(crate::sdk::run), readonly: false, screen: 0 }
    }

    /// Same profile, region and transport, logging its calls to `screen`.
    pub fn on_screen(&self, screen: u64) -> Aws {
        Aws { screen, ..self.in_region(&self.ctx.region) }
    }

    /// Same profile and transport, another region.
    pub fn in_region(&self, region: &str) -> Aws {
        Aws { ctx: Ctx { profile: self.ctx.profile.clone(), region: region.to_string() }, runner: self.runner.clone(), readonly: self.readonly, screen: self.screen }
    }

    #[cfg(test)]
    pub fn with_runner(runner: impl Fn(&Ctx, &str, &str, &[String]) -> Result<Value, AwsError> + Send + Sync + 'static) -> Self {
        Aws { ctx: Ctx { profile: "test".into(), region: "us-west-1".into() }, runner: std::sync::Arc::new(runner), readonly: false, screen: 0 }
    }

    pub fn call(&self, service: &str, op: &str, args: &[String]) -> Result<Value, AwsError> {
        check_read_only(service, op)?;
        self.run(service, op, args)
    }

    fn run(&self, service: &str, op: &str, args: &[String]) -> Result<Value, AwsError> {
        let mut shown = Call::start(self.screen, display_cmd(&self.ctx.region, service, op, args));
        let r = (self.runner)(&self.ctx, service, op, args);
        shown.ok = r.is_ok();
        r
    }

    /// The single write path. Refused under `--readonly`.
    pub fn scale(&self, change: &Scale) -> Result<Value, AwsError> {
        if self.readonly {
            return Err(AwsError::Api("refused: started with --readonly".into()));
        }
        let (service, op, args) = change.command();
        self.run(service, op, &args)
    }
}

/// Every call this session, oldest first: running ones have no `took`. Capped, oldest dropped.
pub struct Entry {
    id: u64,
    pub screen: u64,
    pub cmd: String,
    pub started: std::time::Instant,
    pub took: Option<std::time::Duration>,
    pub ok: bool,
}

static LOG: std::sync::Mutex<Vec<Entry>> = std::sync::Mutex::new(Vec::new());
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const LOG_CAP: usize = 5000;

struct Call {
    id: u64,
    ok: bool,
}

impl Call {
    fn start(screen: u64, cmd: String) -> Self {
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut log = LOG.lock().unwrap();
        if log.len() >= LOG_CAP {
            // Drop the oldest finished call; running ones stay on screen.
            if let Some(i) = log.iter().position(|e| e.took.is_some()) {
                log.remove(i);
            }
        }
        log.push(Entry { id, screen, cmd, started: std::time::Instant::now(), took: None, ok: false });
        Call { id, ok: false }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if let Some(e) = LOG.lock().unwrap().iter_mut().find(|e| e.id == self.id) {
            e.took = Some(e.started.elapsed());
            e.ok = self.ok;
        }
    }
}

/// The commands in flight, oldest first, with how long each has run.
pub fn inflight() -> Vec<(String, std::time::Duration)> {
    LOG.lock().unwrap().iter().filter(|e| e.took.is_none()).map(|e| (e.cmd.clone(), e.started.elapsed())).collect()
}

/// One screen's calls: running first (oldest first), then finished (newest first).
pub fn screen_log(screen: u64) -> Vec<(String, std::time::Duration, Option<bool>, std::time::Instant)> {
    let log = LOG.lock().unwrap();
    let mine = || log.iter().filter(|e| e.screen == screen);
    let running = mine().filter(|e| e.took.is_none()).map(|e| (e.cmd.clone(), e.started.elapsed(), None, e.started));
    let done: Vec<_> = mine().filter_map(|e| e.took.map(|t| (e.cmd.clone(), t, Some(e.ok), e.started))).collect();
    running.chain(done.into_iter().rev()).collect()
}

/// A readable form of a call for the screen: long value lists collapse to the first one plus a count,
/// ARNs to their last segment, JSON to a placeholder. Display only; the real call is untouched.
pub fn display_cmd(region: &str, service: &str, op: &str, args: &[String]) -> String {
    let mut out = vec![format!("aws {service} {op}")];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            let vals: Vec<&String> = args[i + 1..].iter().take_while(|v| !v.starts_with("--")).collect();
            let shown = vals.first().map(|v| {
                let v = if v.starts_with('[') || v.starts_with('{') {
                    "<json>".to_string()
                } else if v.starts_with("arn:") {
                    v.rsplit(['/', ':']).next().unwrap_or(v).to_string()
                } else {
                    v.to_string()
                };
                let v: String = if v.chars().count() > 48 { format!("{}…", v.chars().take(47).collect::<String>()) } else { v };
                if vals.len() > 1 { format!("{v} (+{})", vals.len() - 1) } else { v }
            });
            out.push(match shown {
                Some(v) => format!("{a} {v}"),
                None => a.clone(),
            });
            i += 1 + vals.len();
        } else {
            i += 1;
        }
    }
    out.push(format!("--region {region}"));
    out.join(" ")
}

pub fn classify(stderr: &str, profile: &str, service: &str, op: &str) -> AwsError {
    let l = stderr.to_lowercase();
    let sso = ["token has expired", "error loading sso token", "unauthorizedssotoken", "expiredtoken", "sso session", "token is expired", "refresh failed", "sso token", "loading an access token"];
    if sso.iter().any(|k| l.contains(k)) {
        return AwsError::SsoExpired(profile.to_string());
    }
    let msg = stderr.lines().map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
    AwsError::Api(format!("aws {service} {op}: {msg}"))
}

/// A response that still carries a continuation token was cut short.
pub fn truncated(v: &Value) -> bool {
    ["NextToken", "nextToken", "NextMarker", "Marker", "nextMarker"]
        .iter()
        .any(|k| matches!(v.get(k), Some(Value::String(s)) if !s.is_empty()))
}

/// Profiles and their default regions from ~/.aws/config. Local file, no API call.
/// Profiles from ~/.aws/config plus ~/.aws/credentials, with their configured region. Local files, no API call.
pub fn profiles() -> Vec<(String, Option<String>)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let read = |var: &str, default: String| std::fs::read_to_string(std::env::var(var).unwrap_or(default)).unwrap_or_default();
    let mut out = parse_config(&read("AWS_CONFIG_FILE", format!("{home}/.aws/config")));
    for line in read("AWS_SHARED_CREDENTIALS_FILE", format!("{home}/.aws/credentials")).lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            if !out.iter().any(|(p, _)| p == name) {
                out.push((name.to_string(), None));
            }
        }
    }
    out
}

/// Every profile with its settings (region, sso_account_id, sso_role_name, role_arn...), config first.
pub fn profile_details() -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let read = |var: &str, default: String| std::fs::read_to_string(std::env::var(var).unwrap_or(default)).unwrap_or_default();
    let mut out: Vec<(String, std::collections::BTreeMap<String, String>)> = vec![];
    for (text, is_config) in [(read("AWS_CONFIG_FILE", format!("{home}/.aws/config")), true), (read("AWS_SHARED_CREDENTIALS_FILE", format!("{home}/.aws/credentials")), false)] {
        let mut current: Option<usize> = None;
        for line in text.lines().map(str::trim) {
            if let Some(h) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
                // `[sso-session x]` is kept under its header for now, then folded into its profiles below.
                let name = if !is_config || h == "default" || h.starts_with("sso-session ") { Some(h.trim()) } else { h.strip_prefix("profile ").map(str::trim) };
                current = name.map(|n| match out.iter().position(|(p, _)| p == n) {
                    Some(i) => i,
                    None => {
                        out.push((n.to_string(), Default::default()));
                        out.len() - 1
                    }
                });
            } else if let (Some(i), Some((k, v))) = (current, line.split_once('=')) {
                // Only non-secret settings: credentials files hold keys, which are never read into the UI.
                let k = k.trim();
                if !k.contains("secret") && !k.contains("key") && !k.contains("token") {
                    out[i].1.insert(k.to_string(), v.trim().to_string());
                }
            }
        }
    }
    // A profile using an sso-session gets the session's start URL, which names its login in the token cache.
    let (sessions, mut out): (Vec<_>, Vec<_>) = out.into_iter().partition(|(n, _)| n.starts_with("sso-session "));
    for (_, kv) in &mut out {
        let session = kv.get("sso_session").and_then(|n| sessions.iter().find(|(h, _)| h.strip_prefix("sso-session ").map(str::trim) == Some(n.as_str())));
        if let Some(url) = session.and_then(|(_, s)| s.get("sso_start_url")).cloned() {
            kv.entry("sso_start_url".into()).or_insert(url);
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SsoLogin {
    LoggedIn,
    /// The access token expired, but a refresh token lets the SDK renew it on the next call.
    Renewable,
    LoggedOut,
}

/// The state of the login for an SSO start URL, from the token cache `aws sso login` writes
/// (`~/.aws/sso/cache/*.json`). Local files only; tokens are never read into the UI.
pub fn sso_login(start_url: &str, cache: &std::path::Path, now: chrono::DateTime<chrono::Utc>) -> SsoLogin {
    let mut best = SsoLogin::LoggedOut;
    for f in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        let Ok(v) = std::fs::read_to_string(f.path()).map(|t| serde_json::from_str::<Value>(&t).unwrap_or_default()) else { continue };
        if v["startUrl"].as_str() != Some(start_url) {
            continue;
        }
        let expires = crate::logic::parse_ts(&v["expiresAt"]);
        if expires.is_some_and(|e| e > now) {
            return SsoLogin::LoggedIn;
        }
        if v.get("refreshToken").is_some() {
            best = SsoLogin::Renewable;
        }
    }
    best
}

pub fn sso_cache_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}/.aws/sso/cache", std::env::var("HOME").unwrap_or_default()))
}

/// The profile whose `sso_account_id` is `account`, from ~/.aws/config.
pub fn profile_for_account(account: &str) -> Result<String, String> {
    let path = std::env::var("AWS_CONFIG_FILE").unwrap_or_else(|_| format!("{}/.aws/config", std::env::var("HOME").unwrap_or_default()));
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let (mut current, mut hits) = (None::<String>, vec![]);
    for line in text.lines().map(str::trim) {
        if let Some(h) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            current = if h == "default" { Some(h.into()) } else { h.strip_prefix("profile ").map(|p| p.trim().to_string()) };
        } else if let (Some(p), Some((k, v))) = (&current, line.split_once('=')) {
            if k.trim() == "sso_account_id" && v.trim() == account {
                hits.push(p.clone());
            }
        }
    }
    match hits.len() {
        0 => Err(format!("no profile in ~/.aws/config has sso_account_id {account}")),
        1 => Ok(hits.remove(0)),
        _ => Err(format!("several profiles use account {account}: {}; pick one with --profile", hits.join(", "))),
    }
}

pub fn parse_config(text: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = vec![];
    let mut in_profile = false;
    for line in text.lines().map(str::trim) {
        if let Some(h) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            let name = if h == "default" { Some("default") } else { h.strip_prefix("profile ") };
            in_profile = name.is_some();
            if let Some(n) = name {
                out.push((n.trim().to_string(), None));
            }
        } else if in_profile {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "region" {
                    out.last_mut().unwrap().1 = Some(v.trim().to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sso_login_reads_the_token_cache() {
        let dir = std::env::temp_dir().join(format!("s9s-sso-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = crate::logic::parse_ts(&Value::String("2026-10-15T12:00:00Z".into())).unwrap();
        let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();
        write("a.json", r#"{"startUrl": "https://a/start", "expiresAt": "2026-10-15T13:00:00Z", "accessToken": "x"}"#);
        write("b.json", r#"{"startUrl": "https://b/start", "expiresAt": "2026-10-15T11:00:00Z", "refreshToken": "x"}"#);
        write("c.json", r#"{"startUrl": "https://c/start", "expiresAt": "2026-10-15T11:00:00Z"}"#);
        write("client.json", r#"{"clientId": "x", "expiresAt": "2026-12-01T00:00:00Z"}"#);
        assert_eq!(sso_login("https://a/start", &dir, now), SsoLogin::LoggedIn);
        assert_eq!(sso_login("https://b/start", &dir, now), SsoLogin::Renewable);
        assert_eq!(sso_login("https://c/start", &dir, now), SsoLogin::LoggedOut);
        assert_eq!(sso_login("https://nowhere/start", &dir, now), SsoLogin::LoggedOut);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn display_cmd_collapses_long_argument_lists() {
        let mut args: Vec<String> = vec!["--cluster".into(), "arn:aws:ecs:us-west-1:1:cluster/prod".into(), "--tasks".into()];
        args.extend((0..100).map(|i| format!("arn:aws:ecs:us-west-1:1:task/prod/{i:032}")));
        args.extend(["--metric-data-queries".into(), "[{\"Id\":\"m0\"}]".into()]);
        let shown = display_cmd("us-west-1", "ecs", "describe-tasks", &args);
        assert_eq!(
            shown,
            "aws ecs describe-tasks --cluster prod --tasks 00000000000000000000000000000000 (+99) --metric-data-queries <json> --region us-west-1"
        );
    }
}
