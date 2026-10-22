//! Port forwards, as k9s `shift-f` and `:pf`: a local port tunnelled through an SSM-managed instance (a bastion)
//! to a host it can reach, such as an RDS endpoint. `ssm start-session` goes through `Aws::call` (the one exception
//! the guard allows without `--write`), AWS's session-manager-plugin carries the traffic as it does for the CLI,
//! and `ssm terminate-session` ends it when stopped or when e9s exits.

use crate::aws::{Aws, Ctx, FORWARD_DOC};
use serde_json::{json, Value};
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const PLUGIN_HELP: &str = "port forwarding needs AWS's session-manager-plugin: brew install --cask session-manager-plugin (macOS), \
     or see docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html";

/// What to tunnel to: an RDS endpoint and what psql needs to log in.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub db: String,
    pub user: String,
    pub iam: bool,
}

struct Forward {
    session: String,
    ctx: Ctx,
    bastion: (String, String),
    target: Target,
    local: u16,
    started: chrono::DateTime<chrono::Utc>,
    state: Arc<Mutex<String>>,
    conns: Arc<AtomicUsize>,
    child: Option<Child>,
}

static FORWARDS: Mutex<Vec<Forward>> = Mutex::new(Vec::new());

fn demo() -> bool {
    std::env::var_os("E9S_DEMO").is_some()
}

/// The plugin, on PATH or where AWS's installer puts it.
pub fn plugin() -> Option<std::path::PathBuf> {
    let name = "session-manager-plugin";
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .chain(["/usr/local/sessionmanagerplugin/bin", "/usr/local/bin", "/opt/homebrew/bin"].map(|d| std::path::Path::new(d).join(name)))
        .find(|p| p.is_file())
}

/// `want` if nothing listens there, else the next free port above it.
pub fn free_port(want: u16) -> u16 {
    let taken: Vec<u16> = FORWARDS.lock().unwrap().iter().map(|f| f.local).collect();
    (want..want.saturating_add(200)).find(|p| !taken.contains(p) && port_free(*p)).unwrap_or(want)
}

pub fn port_free(p: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
}

/// Opens the tunnel: StartSession, then the plugin, which listens on `local` and prints its progress.
pub fn start(aws: &Aws, bastion: (String, String), target: Target, local: u16) -> Result<String, String> {
    if FORWARDS.lock().unwrap().iter().any(|f| f.local == local) || !port_free(local) {
        return Err(format!("localhost:{local} is in use: pick another local port"));
    }
    let plugin = if demo() { None } else { Some(plugin().ok_or(PLUGIN_HELP)?) };
    let params = format!("host={},portNumber={},localPortNumber={local}", target.host, target.port);
    let args = ["--target", &bastion.0, "--document-name", FORWARD_DOC, "--parameters", &params].map(String::from);
    let r = aws.call("ssm", "start-session", &args).map_err(|e| e.to_string())?;
    let session = r["SessionId"].as_str().unwrap_or_default().to_string();
    let (state, conns) = (Arc::new(Mutex::new("starting".to_string())), Arc::new(AtomicUsize::new(0)));
    let child = match plugin {
        None => {
            *state.lock().unwrap() = "listening".into();
            None
        }
        Some(bin) => {
            let region = &aws.ctx.region;
            let request = json!({
                "Target": bastion.0, "DocumentName": FORWARD_DOC,
                "Parameters": {"host": [target.host], "portNumber": [target.port.to_string()], "localPortNumber": [local.to_string()]},
            });
            // The arguments the AWS CLI hands the plugin.
            let spawned = Command::new(bin)
                .args([r.to_string(), region.clone(), "StartSession".into(), aws.ctx.profile.clone(), request.to_string(), format!("https://ssm.{region}.amazonaws.com")])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match spawned {
                Ok(c) => c,
                Err(e) => {
                    let _ = terminate(aws, &session);
                    return Err(format!("session-manager-plugin: {e}"));
                }
            };
            watch(child.stdout.take(), child.stderr.take(), state.clone(), conns.clone());
            Some(child)
        }
    };
    FORWARDS.lock().unwrap().push(Forward { session: session.clone(), ctx: aws.ctx.clone(), bastion, target, local, started: chrono::Utc::now(), state, conns, child });
    Ok(session)
}

/// The plugin's output, read into the STATE column: "Waiting for connections..." once the port is open,
/// "Connection accepted" per client, and its last error line when it exits.
fn watch(out: Option<std::process::ChildStdout>, err: Option<std::process::ChildStderr>, state: Arc<Mutex<String>>, conns: Arc<AtomicUsize>) {
    let last_err = Arc::new(Mutex::new(String::new()));
    if let Some(err) = err {
        let last_err = last_err.clone();
        std::thread::spawn(move || {
            for l in std::io::BufReader::new(err).lines().map_while(Result::ok).filter(|l| !l.trim().is_empty()) {
                *last_err.lock().unwrap() = l.trim().to_string();
            }
        });
    }
    let Some(out) = out else { return };
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if l.contains("Waiting for connections") {
                *state.lock().unwrap() = "listening".into();
            } else if l.contains("Connection accepted") {
                conns.fetch_add(1, Ordering::Relaxed);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        let why = last_err.lock().unwrap().clone();
        let mut st = state.lock().unwrap();
        if st.as_str() != "stopped" {
            *st = if why.is_empty() { "ended".into() } else { format!("ended: {why}") };
        }
    });
}

fn terminate(aws: &Aws, session: &str) -> Result<(), String> {
    aws.call("ssm", "terminate-session", &["--session-id".into(), session.into()]).map(|_| ()).map_err(|e| e.to_string())
}

/// Stops one forward: the plugin first, so the port closes now, then the session on AWS's side.
pub fn stop(aws: &Aws, session: &str) -> Result<(), String> {
    let f = {
        let mut all = FORWARDS.lock().unwrap();
        let i = all.iter().position(|f| f.session == session).ok_or("that forward is gone")?;
        all.remove(i)
    };
    end(aws, f)
}

fn end(aws: &Aws, mut f: Forward) -> Result<(), String> {
    *f.state.lock().unwrap() = "stopped".into();
    if let Some(mut c) = f.child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    terminate(aws, &f.session)
}

/// When e9s exits: every forward ends, in parallel. Returns one line per forward, said after the screen closes.
pub fn stop_all() -> Vec<String> {
    let all: Vec<Forward> = std::mem::take(&mut *FORWARDS.lock().unwrap());
    std::thread::scope(|sc| {
        let hs: Vec<_> = all
            .into_iter()
            .map(|f| {
                sc.spawn(move || {
                    let what = format!("localhost:{} -> {}:{}", f.local, f.target.host, f.target.port);
                    match end(&Aws::new(f.ctx.clone()), f) {
                        Ok(()) => format!("e9s: port forward {what} ended"),
                        Err(e) => format!("e9s: port forward {what}: its session could not be ended ({e}); it times out on its own"),
                    }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    })
}

/// The forwards of this run, oldest first, as rows for `:pf` (never the session token).
pub fn list() -> Vec<Value> {
    FORWARDS
        .lock()
        .unwrap()
        .iter()
        .map(|f| json!({
            "SessionId": f.session, "Profile": f.ctx.profile, "Region": f.ctx.region,
            "Bastion": f.bastion.0, "BastionName": f.bastion.1, "Target": f.target.name, "Host": f.target.host, "Port": f.target.port,
            "LocalPort": f.local, "Started": f.started.to_rfc3339(), "State": *f.state.lock().unwrap(),
            "Connections": f.conns.load(Ordering::Relaxed), "Database": f.target.db, "User": f.target.user, "IAM": f.target.iam,
        }))
        .collect()
}

/// The forward a session id names, for psql and the IAM token.
pub fn get(session: &str) -> Option<(Ctx, Target, u16)> {
    FORWARDS.lock().unwrap().iter().find(|f| f.session == session).map(|f| (f.ctx.clone(), f.target.clone(), f.local))
}

/// The local port forwarded to `host` in this profile and region, for the endpoint list's FORWARD column.
pub fn local_for(ctx: &Ctx, host: &str) -> Option<u16> {
    FORWARDS.lock().unwrap().iter().find(|f| &f.ctx == ctx && f.target.host == host).map(|f| f.local)
}

// ---------- remembered choices: the last bastion per profile and region, the last db user per host ----------

static PREFS: Mutex<Option<Vec<(String, String)>>> = Mutex::new(None);

fn prefs_file() -> std::path::PathBuf {
    crate::aws::home().join(".config/e9s/remembered")
}

/// A remembered value. The file holds `key<TAB>value` lines; in demo mode nothing is read or written.
pub fn recall(key: &str) -> Option<String> {
    let mut p = PREFS.lock().unwrap();
    let all = p.get_or_insert_with(|| {
        let text = if demo() { String::new() } else { std::fs::read_to_string(prefs_file()).unwrap_or_default() };
        text.lines().filter_map(|l| l.split_once('\t')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
    });
    all.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

pub fn remember(key: &str, value: &str) {
    recall(key);
    let mut p = PREFS.lock().unwrap();
    let all = p.as_mut().expect("loaded by recall");
    all.retain(|(k, _)| k != key);
    all.push((key.to_string(), value.to_string()));
    if !demo() {
        let text: String = all.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
        let f = prefs_file();
        let _ = f.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(f, text);
    }
}

pub fn bastion_key(ctx: &Ctx) -> String {
    format!("bastion {} {}", ctx.profile, ctx.region)
}

/// Whether an instance reads as a bastion by its name or tags.
pub fn likely_bastion(words: &[String]) -> bool {
    words.iter().any(|w| {
        let w = w.to_lowercase();
        ["bastion", "jump", "tunnel", "proxy"].iter().any(|k| w.contains(k))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bastions_by_name_or_tag() {
        let w = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(likely_bastion(&w(&["prod-Bastion-1"])));
        assert!(likely_bastion(&w(&["api-1", "role=jumphost"])));
        assert!(!likely_bastion(&w(&["ci-runner-1", "vpn-gateway"])));
    }

    #[test]
    fn free_port_skips_a_taken_one() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        assert!(!port_free(p));
        assert_ne!(free_port(p), p);
    }
}
