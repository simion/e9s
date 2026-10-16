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
            AwsError::SsoExpired(p) if p.is_empty() => write!(f, "no credentials, or they expired: set them up (aws configure, aws sso login), then press r"),
            AwsError::SsoExpired(p) => write!(f, "credentials for profile {p} expired or are missing: press L to log in, or renew them and press r"),
            AwsError::Api(m) => f.write_str(m),
        }
    }
}

const SERVICES: &[&str] = &[
    "ecs", "logs", "elbv2", "application-autoscaling", "ec2", "ecr", "sqs", "rds", "lambda", "cloudwatch", "sts", "cloudtrail",
];
const READ_PREFIXES: &[&str] = &["describe-", "list-", "get-"];
const READ_EXACT: &[&str] = &["filter-log-events", "lookup-events"];
// Reads that hand out credentials. Not mutations, but nothing here needs them.
const DENIED: &[&str] = &["get-login-password", "get-authorization-token", "get-session-token", "get-federation-token", "get-password-data"];

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
    /// The screen whose command log records these calls (0: none).
    pub screen: u64,
}

impl Aws {
    pub fn new(ctx: Ctx) -> Self {
        // E9S_DEMO=1: a made-up account answers, for screenshots and recordings.
        let runner: Runner = if std::env::var_os("E9S_DEMO").is_some() { std::sync::Arc::new(crate::demo::run) } else { std::sync::Arc::new(crate::sdk::run) };
        Aws { ctx, runner, screen: 0 }
    }

    /// Same profile, region and transport, logging its calls to `screen`.
    pub fn on_screen(&self, screen: u64) -> Aws {
        Aws { screen, ..self.in_region(&self.ctx.region) }
    }

    /// Same profile and transport, another region.
    pub fn in_region(&self, region: &str) -> Aws {
        Aws { ctx: Ctx { profile: self.ctx.profile.clone(), region: region.to_string() }, runner: self.runner.clone(), screen: self.screen }
    }

    #[cfg(test)]
    pub fn with_runner(runner: impl Fn(&Ctx, &str, &str, &[String]) -> Result<Value, AwsError> + Send + Sync + 'static) -> Self {
        Aws { ctx: Ctx { profile: "test".into(), region: "us-west-1".into() }, runner: std::sync::Arc::new(runner), screen: 0 }
    }

    pub fn call(&self, service: &str, op: &str, args: &[String]) -> Result<Value, AwsError> {
        check_read_only(service, op)?;
        let mut shown = Call::start(self.screen, display_cmd(&self.ctx.region, service, op, args));
        let r = (self.runner)(&self.ctx, service, op, args);
        shown.ok = r.is_ok();
        r
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
    let sso = ["token has expired", "error loading sso token", "unauthorizedssotoken", "expiredtoken", "sso session", "token is expired", "refresh failed", "sso token", "loading an access token", "no providers in chain", "credentials have expired", "login token", ".aws/sso/cache/", ".aws/login/cache/"];
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
                // That keys exist is kept (it is how the profile signs in), and the web identity token's path.
                let k = k.trim();
                if k == "aws_access_key_id" {
                    out[i].1.insert("static_keys".into(), "yes".into());
                } else if k == "web_identity_token_file" || (!k.contains("secret") && !k.contains("key") && !k.contains("token")) {
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

type Profiles = [(String, std::collections::BTreeMap<String, String>)];

/// How a profile gets credentials, in the order the SDK checks them.
#[derive(Clone, Debug, PartialEq)]
pub enum Auth {
    /// IAM Identity Center; `session` is the sso-session name, absent for the older per-profile settings.
    Sso { session: Option<String>, start_url: String },
    /// `aws login` (console credentials), `login_session = <arn>`.
    Console { session: String },
    /// `role_arn` assumed with another profile's credentials; `mfa` roles need a code the SDK cannot ask for.
    Role { source: String, mfa: bool },
    Keys,
    Process,
    WebIdentity,
    /// Nothing in the files: environment variables, container or instance credentials.
    Unknown,
}

pub fn auth_of(profile: &str, all: &Profiles) -> Auth {
    let Some((_, kv)) = all.iter().find(|(p, _)| p == profile) else { return Auth::Unknown };
    let get = |k: &str| kv.get(k).cloned();
    if get("role_arn").is_some_and(|r| !r.is_empty()) {
        if kv.contains_key("web_identity_token_file") {
            return Auth::WebIdentity;
        }
        let source = get("source_profile").or_else(|| get("credential_source")).unwrap_or_default();
        return Auth::Role { source, mfa: kv.contains_key("mfa_serial") };
    }
    if let Some(url) = get("sso_start_url") {
        return Auth::Sso { session: get("sso_session"), start_url: url };
    }
    if let Some(arn) = get("login_session") {
        return Auth::Console { session: arn };
    }
    if kv.contains_key("static_keys") {
        Auth::Keys
    } else if kv.contains_key("credential_process") {
        Auth::Process
    } else {
        Auth::Unknown
    }
}

/// The profile to log in for `profile` and how: itself for SSO or `aws login`, the source profile for a role.
pub fn login_target(profile: &str, all: &Profiles) -> Option<(String, Auth)> {
    let mut p = profile.to_string();
    for _ in 0..8 {
        match auth_of(&p, all) {
            Auth::Role { source, mfa: false } if !source.is_empty() && source != p => p = source,
            a @ (Auth::Sso { .. } | Auth::Console { .. }) => return Some((p, a)),
            _ => return None,
        }
    }
    None
}

/// The CLI command that logs `profile` in, without the leading `aws`.
pub fn login_args(profile: &str, auth: &Auth) -> Vec<String> {
    let verb: &[&str] = if matches!(auth, Auth::Console { .. }) { &["login"] } else { &["sso", "login"] };
    verb.iter().map(|s| s.to_string()).chain(["--profile".to_string(), profile.to_string()]).collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LoginState {
    LoggedIn,
    /// The access token expired, but a refresh token lets the SDK renew it on the next call.
    Renewable,
    LoggedOut,
}

/// The state of a login, read from the exact cache file the SDK reads (the CLI writes it):
/// `~/.aws/sso/cache/<sha1 of session name, or of start URL>.json`, `~/.aws/login/cache/<sha256 of ARN>.json`.
/// Local files only; tokens are never read into the UI.
pub fn login_state(auth: &Auth, home: &std::path::Path, now: chrono::DateTime<chrono::Utc>) -> Option<LoginState> {
    use sha1::Digest;
    let (path, expires_ptr) = match auth {
        Auth::Sso { session, start_url } => {
            let id = session.as_deref().unwrap_or(start_url);
            (home.join(format!(".aws/sso/cache/{:x}.json", sha1::Sha1::digest(id.as_bytes()))), "/expiresAt")
        }
        Auth::Console { session } => {
            (home.join(format!(".aws/login/cache/{:x}.json", sha2::Sha256::digest(session.trim().as_bytes()))), "/accessToken/expiresAt")
        }
        _ => return None,
    };
    let Ok(v) = std::fs::read_to_string(path).map(|t| serde_json::from_str::<Value>(&t).unwrap_or_default()) else {
        return Some(LoginState::LoggedOut);
    };
    Some(if v.pointer(expires_ptr).and_then(crate::logic::parse_ts).is_some_and(|e| e > now) {
        LoginState::LoggedIn
    } else if v.get("refreshToken").is_some() {
        LoginState::Renewable
    } else {
        LoginState::LoggedOut
    })
}

pub fn home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
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
    fn auth_methods_and_login_state() {
        use std::collections::BTreeMap;
        let p = |name: &str, kv: &[(&str, &str)]| (name.to_string(), kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>());
        let all = vec![
            p("sso", &[("sso_session", "corp"), ("sso_start_url", "https://corp/start")]),
            p("legacy", &[("sso_start_url", "https://old/start")]),
            p("console", &[("login_session", "arn:aws:iam::1:user/me")]),
            p("role", &[("role_arn", "arn:aws:iam::1:role/r"), ("source_profile", "sso")]),
            p("mfa", &[("role_arn", "arn:aws:iam::1:role/r"), ("source_profile", "sso"), ("mfa_serial", "arn:aws:iam::1:mfa/me")]),
            p("keys", &[("static_keys", "yes")]),
            p("proc", &[("credential_process", "x")]),
            p("web", &[("role_arn", "arn:aws:iam::1:role/r"), ("web_identity_token_file", "/t")]),
        ];
        assert!(matches!(auth_of("legacy", &all), Auth::Sso { session: None, .. }));
        assert_eq!(auth_of("keys", &all), Auth::Keys);
        assert_eq!(auth_of("proc", &all), Auth::Process);
        assert_eq!(auth_of("web", &all), Auth::WebIdentity);
        // A role logs in through its source profile; an MFA role cannot be driven by the SDK.
        assert_eq!(login_target("role", &all).map(|t| t.0), Some("sso".into()));
        assert_eq!(login_target("mfa", &all), None);
        assert_eq!(login_target("keys", &all), None);
        let (who, how) = login_target("console", &all).unwrap();
        assert_eq!(login_args(&who, &how), ["login", "--profile", "console"]);
        assert_eq!(login_args("sso", &auth_of("sso", &all)), ["sso", "login", "--profile", "sso"]);

        // The cache file names are the SDK's: sha1 of the session name (or start URL), sha256 of the login ARN.
        let home = std::env::temp_dir().join(format!("e9s-home-{}", std::process::id()));
        for d in [".aws/sso/cache", ".aws/login/cache"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let now = crate::logic::parse_ts(&Value::String("2026-10-15T12:00:00Z".into())).unwrap();
        use sha1::Digest;
        let sso_file = home.join(format!(".aws/sso/cache/{:x}.json", sha1::Sha1::digest(b"corp")));
        std::fs::write(&sso_file, r#"{"startUrl": "https://corp/start", "expiresAt": "2026-10-15T13:00:00Z"}"#).unwrap();
        assert_eq!(login_state(&auth_of("sso", &all), &home, now), Some(LoginState::LoggedIn));
        std::fs::write(&sso_file, r#"{"expiresAt": "2026-10-15T11:00:00Z", "refreshToken": "x"}"#).unwrap();
        assert_eq!(login_state(&auth_of("sso", &all), &home, now), Some(LoginState::Renewable));
        // The legacy profile's token lives under the hash of its start URL, which is not there.
        assert_eq!(login_state(&auth_of("legacy", &all), &home, now), Some(LoginState::LoggedOut));
        let console_file = home.join(format!(".aws/login/cache/{:x}.json", sha2::Sha256::digest(b"arn:aws:iam::1:user/me")));
        std::fs::write(&console_file, r#"{"accessToken": {"expiresAt": "2026-10-15T13:00:00Z"}}"#).unwrap();
        assert_eq!(login_state(&auth_of("console", &all), &home, now), Some(LoginState::LoggedIn));
        assert_eq!(login_state(&Auth::Keys, &home, now), None);
        std::fs::remove_dir_all(&home).unwrap();
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
