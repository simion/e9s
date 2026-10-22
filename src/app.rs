//! App state and key handling. Fetches run on background threads and come back as `Msg`.

use crate::aws::{Aws, AwsError, Ctx};
use crate::logic::{short, sort_key, to_yaml, Health};
use crate::views::*;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::time::{Duration, Instant};

pub enum Msg {
    Loaded { seq: u64, result: Result<Table, AwsError> },
    Identity { ctx: Ctx, result: Result<Value, AwsError> },
    Tail { seq: u64, rows: Vec<Row>, skipped: bool },
    TailEnded { seq: u64, err: String },
}

pub enum Mode {
    Normal,
    Command(String),
    Filter(String),
    Pattern(String),
    /// Help overlay, scrolled by this many lines.
    Help(u16),
    /// This screen's AWS calls, scrolled by this many lines.
    Calls(u16),
    /// The login expired: offer to run the login command for this profile.
    OfferLogin(Login),
    /// `e` on a secret's key, `s` on a service or scaling target: the new value being typed.
    Value(String),
    /// A write waiting for y.
    /// The bool: OK has focus (k9s starts on Cancel).
    Confirm(Pending, bool),
    /// `F` on a database endpoint: the bastion and local port to forward through (k9s shift-f).
    Forward(ForwardForm),
}

/// The port-forward dialog: ↑↓ pick the bastion, digits change the local port, Enter starts.
pub struct ForwardForm {
    pub target: crate::forward::Target,
    /// Online SSM instances, likely bastions first: (instance id, name, likely bastion).
    pub bastions: Vec<(String, String, bool)>,
    pub pick: usize,
    pub local: String,
    /// The suggested port is still untouched: the first digit typed replaces it.
    pub fresh: bool,
}

/// psql through a forward, on the real terminal: its environment (PGPASSWORD holds the IAM token, never an argument).
pub struct Psql {
    pub env: Vec<(String, String)>,
    pub what: String,
}

/// What the value prompt is taking.
pub enum Editing {
    SecretKey(Secret, Option<String>),
    /// A scaling target's row: its JSON and the RESTORE cell.
    Range(Row),
    Desired { cluster: String, service: String, now: i64 },
    /// `s` (psql) or `t` (copy an IAM token) on a port forward: the db user and database.
    DbUser { session: String, copy: bool },
}

/// A write staged until y: what the dialog says, and what it does.
pub struct Pending {
    pub title: String,
    /// Names and numbers only, never a secret's value.
    pub changes: Vec<String>,
    pub note: String,
    write: Write,
    /// Where to go once it is done (a restart: its rollout watch).
    then: Option<View>,
}

impl Pending {
    /// The dialog's frame title.
    pub fn heading(&self) -> &'static str {
        match self.write {
            Write::StopForward(_) => " Port forward ",
            _ => " Write to AWS ",
        }
    }
}

enum Write {
    /// Not a write: ends a port forward this run started.
    StopForward(String),
    Secret { read: Secret, text: String },
    /// Calls run in order, stopping at the first failure.
    Calls(Vec<(&'static str, &'static str, Vec<String>)>),
}

/// A login to run: the profile to log in and the `aws` arguments that do it.
#[derive(Clone, Debug, PartialEq)]
pub struct Login {
    pub profile: String,
    pub args: Vec<String>,
}

pub const COMMANDS: &[(&[&str], &str)] = &[
    (&["clusters", "cl", "ecs", "ns"], "ECS clusters"),
    (&["services", "svc", "deploy", "dp"], "ECS services of [cluster] (default: current)"),
    (&["tasks", "task", "po"], "ECS tasks of [cluster], running and recently stopped"),
    (&["events", "ev"], "events of the current service"),
    (&["rollout", "ro"], "rollout watch of the current service"),
    (&["failures", "fail"], "failed tasks and problem events of the last hour, whole cluster"),
    (&["taskdefs", "td"], "task definition families"),
    (&["logs", "lg"], "log groups; :logs <group> [filter pattern] opens events"),
    (&["lb", "loadbalancers", "alb", "elb", "ing"], "load balancers"),
    (&["tg", "targetgroups"], "all target groups"),
    (&["scaling", "as", "hpa"], "scalable targets [namespace, default ecs]"),
    (&["policies", "pol"], "scaling policies [namespace]"),
    (&["activities", "act"], "scaling activities [namespace]"),
    (&["ec2", "no", "nodes"], "EC2 instances"),
    (&["ecr"], "ECR repositories"),
    (&["sqs"], "SQS queues"),
    (&["rds"], "RDS clusters and instances"),
    (&["endpoints", "ep", "db"], "RDS endpoints (writer, reader, custom, instance): F forwards one through a bastion"),
    (&["ssm", "bastions", "bastion"], "SSM-managed instances, likely bastions first"),
    (&["portforwards", "pf", "forwards"], "port forwards of this run (k9s :pf): stop, psql, IAM token"),
    (&["lambda", "fn"], "Lambda functions"),
    (&["pulse", "pu", "overview"], "everything not healthy: alarms, ECS services, RDS, Lambda"),
    (&["alarms", "alarm", "al"], "CloudWatch alarms, firing first"),
    (&["trail", "ct", "changes"], "CloudTrail write events of the last 24h, or :trail <resource name> for its last 7 days"),
    (&["access", "can", "perms"], "which reads this profile may make"),
    (&["secrets", "secret", "sm"], "Secrets Manager secrets; :secrets <name> opens one (values need --write)"),
    (&["profile", "ctx"], "pick a profile, or :profile <name>"),
    (&["region", "regions"], "regions with resource counts, or :region <name>"),
    (&["quit", "q"], "quit"),
];

pub const QUICK: &[(char, &str)] = &[
    ('1', "Clusters"), ('2', "Services"), ('3', "Tasks"), ('4', "Logs"), ('5', "LoadBalancers"),
    ('6', "TargetGroups"), ('7', "Scaling"), ('8', "ECR"), ('9', "SQS"), ('0', "RDS"),
];

pub const KEYS: &[(&str, &str)] = &[
    (":", "command mode (Tab completes, Up/Down pick a suggestion)"),
    ("ctrl-a", "every command and its aliases"),
    ("ctrl-z", "faults: only rows that are not healthy (red or yellow), on or off"),
    ("/", "filter rows live: regex, case-insensitive, !re inverts, -f fuzzy; in describe it searches (n N next/prev)"),
    ("Esc", "clear filter, else back one level"),
    ("q", "back one level; quits at the root"),
    ("ctrl-c", "quit"),
    ("Enter", "drill into the selected row"),
    ("j k / ↑ ↓", "move"),
    ("g G", "top / bottom"),
    ("ctrl-f ctrl-b", "page down / up (also PgDn PgUp, ctrl-u)"),
    ("d", "describe: full detail of the selection"),
    ("y", "YAML of the selection: the fields e9s reads, under their API names"),
    ("l", "logs for the selection"),
    ("s", ":pf: psql through the selected forward (with an IAM token when the database has IAM auth) · logs: autoscroll (follow new lines) on / off · services: desired count · scaling: min and max, `4 20 for 2h` puts the old ones back after 2h (--write)"),
    ("f", "logs, describe: fullscreen · endpoints: show its port forward (k9s f)"),
    ("t", "logs: show or hide timestamps · :pf: copy an RDS IAM auth token for the forward's database"),
    ("0-6", "logs: start from the tail, the head, or 1m 5m 15m 30m 1h back (elsewhere 0-9 are quick views)"),
    ("a", "service logs: all containers of each task, not just the main one"),
    ("m C c", "logs: draw a mark line, clear the screen, copy the lines shown"),
    ("Shift-F", "failures of the last hour: failed tasks and problem events (service, or whole cluster) · endpoints: port-forward through an SSM bastion (k9s Shift-F)"),
    ("ctrl-d", ":pf: stop the selected port forward"),
    ("p", "logs: filter pattern · scaling: policies"),
    ("w", "wrap lines (logs, describe)"),
    ("ctrl-s", "save the current view to ~/Downloads (logs: every line so far, as .log)"),
    ("R", "rollout watch of the selected service"),
    ("e", "events of the selected service · secret: edit the selected value (--write)"),
    ("E x", "secret: edit the whole secret in $EDITOR, show or hide values (--write)"),
    ("m", "metrics (services, load balancers, target groups, RDS, Lambda)"),
    ("T", "who changed it: CloudTrail events of the selected RDS, Lambda or EC2 resource"),
    ("r ctrl-r", "refresh now · with --write, r on a service restarts it (new tasks, same task definition) and ctrl-r refreshes, as in k9s"),
    ("L", "log in (aws sso login or aws login): the selected profile in :profile, else the current one; a role logs in its source profile"),
    ("c", "copy the command that reopens this view (logs: the lines shown; describe: the page)"),
    ("+", "auto-refresh interval: off, 5s, 10s, 15s, 30s, 60s, then off again"),
    ("[ ]", "back / forward through the commands typed after :"),
    ("-", "the command before this one (again: back)"),
    ("< >", "sort by previous / next column"),
    ("o", "reverse sort order"),
    ("N A S", "sort by name / age / status"),
    ("ctrl-e ctrl-g", "hide or show the header / the breadcrumbs"),
    ("0-9", "quick views (header shows the map)"),
    ("H", "AWS calls this screen made, as aws CLI commands: running, then finished, with time and result"),
    ("?", "this help"),
];

const INTERVALS: &[u64] = &[0, 5, 10, 15, 30, 60];
const TAIL_CAP: usize = 50_000;
const TAIL_EVERY: Duration = Duration::from_secs(2);

static NEXT_SCREEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct Frame {
    /// Keys this screen's command log.
    pub id: u64,
    pub view: View,
    pub table: Table,
    pub sel: usize,
    pub offset: usize,
    pub filter: String,
    pub sort: Option<(usize, bool)>,
    pub loading: bool,
    /// When the current fetch started, for the spinner's elapsed time.
    pub load_started: Instant,
    pub loaded: bool,
    pub error: Option<AwsError>,
    pub seq: u64,
    pub wrap: bool,
    /// Logs: show each line's time (k9s `t`).
    pub show_time: bool,
    /// Logs: k9s fullscreen (`f`), header and breadcrumbs hidden.
    pub full: bool,
    /// k9s ctrl-z: only rows that are not healthy (red or yellow).
    pub faults: bool,
    /// Set while live tail polls; clearing it stops the poller.
    pub tail: Option<Arc<AtomicBool>>,
}

impl Frame {
    fn new(view: View) -> Self {
        Frame {
            id: NEXT_SCREEN.fetch_add(1, Ordering::Relaxed), view, table: Table::default(), sel: 0, offset: 0, filter: String::new(), sort: None, loading: false, load_started: Instant::now(),
            loaded: false, error: None, seq: 0, wrap: true, show_time: true, full: false, faults: false, tail: None,
        }
    }

    /// Row indexes after filter and sort, in display order.
    pub fn visible(&self) -> Vec<usize> {
        let rows = &self.table.rows;
        // Describe keeps every line and highlights matches (k9s details search); the rest filter.
        let re = filter_re(&self.filter).filter(|_| !matches!(self.view, View::Detail { .. }));
        let mut idx: Vec<usize> = (0..rows.len())
            .filter(|&i| !self.faults || matches!(rows[i].health, Health::Bad | Health::Warn))
            .filter(|&i| re.as_ref().is_none_or(|(r, inv)| r.is_match(&rows[i].cells.join(" ")) != *inv))
            .collect();
        if let Some((c, desc)) = self.sort {
            let cell = |i: usize| sort_key(rows[i].cells.get(c).map(String::as_str).unwrap_or(""));
            idx.sort_by(|&a, &b| cell(a).partial_cmp(&cell(b)).unwrap_or(std::cmp::Ordering::Equal));
            if desc {
                idx.reverse();
            }
        }
        idx
    }

    pub fn selected(&self) -> Option<&Row> {
        self.visible().get(self.sel).map(|&i| &self.table.rows[i])
    }

    /// Describe search: put the next (or previous) line matching the filter at the top. False if none.
    fn jump_match(&mut self, from: usize, forward: bool) -> bool {
        let Some((re, _)) = filter_re(&self.filter) else { return false };
        let n = self.table.rows.len();
        let hit = (0..n)
            .map(|k| if forward { (from + k) % n } else { (from + n - k) % n })
            .find(|&i| re.is_match(&self.table.rows[i].cells.join(" ")));
        if let Some(i) = hit {
            self.sel = i;
            self.offset = i;
        }
        hit.is_some()
    }

    fn stop_tail(&mut self) {
        if let Some(run) = self.tail.take() {
            run.store(false, Ordering::Relaxed);
        }
    }
}

pub fn filter_re(f: &str) -> Option<(Regex, bool)> {
    if f.is_empty() || f == "!" {
        return None;
    }
    let (inv, pat) = f.strip_prefix('!').map_or((false, f), |p| (true, p));
    // k9s `-f`: fuzzy, the letters in order with anything between.
    if let Some(fz) = pat.strip_prefix("-f ").map(str::trim).filter(|p| !p.is_empty()) {
        let re = fz.chars().map(|c| regex::escape(&c.to_string())).collect::<Vec<_>>().join(".*?");
        return Regex::new(&format!("(?i){re}")).ok().map(|r| (r, inv));
    }
    Regex::new(&format!("(?i){pat}"))
        .or_else(|_| Regex::new(&format!("(?i){}", regex::escape(pat))))
        .ok()
        .map(|r| (r, inv))
}

/// What the command line asked for.
#[derive(Default)]
pub struct Start {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub cluster: Option<String>,
    pub service: Option<String>,
    pub command: Option<String>,
    pub refresh: Option<u64>,
}

/// Commands ignore case, so `:LoadBalancers` works like `:loadbalancers`.
pub fn canonical(word: &str) -> Option<&'static str> {
    let word = word.to_lowercase();
    COMMANDS.iter().find(|(a, _)| a.contains(&word.as_str())).map(|(a, _)| a[0])
}

pub struct App {
    /// Which parts of the screen to hide (--headless, --logoless, --crumbsless).
    pub chrome: crate::ui::Chrome,
    /// Command mode: the typed prefix and which suggestion Up/Down picked.
    pub pick: Option<(String, usize)>,
    /// k9s command history: commands typed after `:`, where `[` `]` are, and where `-` returns to.
    pub history: Vec<String>,
    hist_at: usize,
    hist_last: usize,
    pub aws: Arc<Aws>,
    pub arn: Option<String>,
    pub account: Option<String>,
    pub ident_err: Option<String>,
    pub sso_expired: bool,
    pub stack: Vec<Frame>,
    pub mode: Mode,
    pub interval: u64,
    pub next_refresh: Instant,
    pub flash: Option<(String, bool, Instant)>,
    pub page: usize,
    pub quit: bool,
    /// A login to run (`aws sso login` or `aws login`); the main loop pauses the screen to run it.
    pub login: Option<Login>,
    /// The expired login was already offered; asked again only after credentials worked once more.
    offered: bool,
    /// When credentials expired: the login that would renew them, for the header (None: nothing to run).
    pub expired_login: Option<Login>,
    /// `E`: a secret to edit in $EDITOR; the main loop hands the terminal over.
    pub editor: Option<Secret>,
    /// What the value prompt (`Mode::Value`) is for.
    pub editing: Option<Editing>,
    /// psql to run; the main loop hands the terminal over.
    pub psql: Option<Psql>,
    /// What `shutdown` did with the port forwards, printed once the screen is gone.
    pub ended: Vec<String>,
    cluster: Option<String>,
    service: Option<String>,
    tx: Sender<Msg>,
    seq: u64,
}

impl App {
    /// k9s-style start. Without flags: pick a profile, then a region, then clusters.
    /// Flags jump ahead; the screens in between are stacked (not loaded) so Esc walks back up.
    pub fn new(start: Start, tx: Sender<Msg>) -> Self {
        let known = crate::aws::profiles();
        let pick_profile = start.profile.is_none() && known.len() > 1;
        // A single configured profile is used as is; none means the default credential chain.
        let profile = start.profile.clone().or_else(|| (known.len() == 1).then(|| known[0].0.clone())).unwrap_or_default();
        // A cluster, service or command implies a region: the profile's own when none is given.
        let jump = start.cluster.is_some() || start.command.is_some();
        let region = start.region.clone().or_else(|| jump.then(|| default_region(&profile)));
        let home = region.clone().unwrap_or_else(|| default_region(&profile));
        let mut app = App {
            chrome: Default::default(), aws: Arc::new(Aws::new(Ctx { profile, region: home })), arn: None, account: None, ident_err: None, sso_expired: false,
            stack: vec![], mode: Mode::Normal, pick: None, history: vec![], hist_at: 0, hist_last: 0, interval: start.refresh.unwrap_or(10), next_refresh: Instant::now(), flash: None, page: 20,
            quit: false, login: None, offered: false, expired_login: None, editor: None, editing: None, psql: None, ended: vec![], cluster: start.cluster.clone(), service: start.service.clone(), tx, seq: 0,
        };
        if pick_profile {
            app.replace_root(View::Profiles);
            return app;
        }
        app.identity();
        if region.is_none() {
            app.replace_root(View::Regions);
        } else {
            let mut views = vec![View::Clusters];
            if let Some(c) = &start.cluster {
                views.push(View::Services { cluster: c.clone() });
                if let Some(s) = &start.service {
                    views.push(View::Tasks { cluster: c.clone(), service: Some(s.clone()) });
                }
            }
            app.stack = views.into_iter().map(Frame::new).collect();
            match &start.command {
                // A service-scoped command keeps the trail above it.
                Some(cmd) if matches!(canonical(cmd.split_whitespace().next().unwrap_or("")), Some("events" | "rollout" | "failures")) && app.service.is_some() => {
                    let (c, s) = (app.cluster.clone().unwrap_or_default(), app.service.clone().unwrap_or_default());
                    app.push(match canonical(cmd.split_whitespace().next().unwrap_or("")) {
                        Some("events") => View::Events { cluster: c, service: s },
                        Some("failures") => View::Failures { cluster: c, service: Some(s) },
                        _ => View::Rollout { cluster: c, service: s },
                    });
                }
                Some(cmd) => {
                    // Any other command opens its view as `:cmd` would, with the screens you would
                    // have walked through stacked under it.
                    app.exec(cmd);
                    let parents = parents_of(&app.top().view);
                    let at = app.stack.len() - 1;
                    app.stack.splice(at..at, parents.into_iter().map(Frame::new));
                }
                None => app.load(),
            }
        }
        // Above it all, the region list and the profile picker, so Esc walks all the way back.
        let mut above = vec![];
        if known.len() > 1 && !matches!(app.stack[0].view, View::Profiles) {
            above.push(View::Profiles);
        }
        if !matches!(app.stack[0].view, View::Profiles | View::Regions) {
            above.push(View::Regions);
        }
        app.stack.splice(0..0, above.into_iter().map(Frame::new));
        app
    }

    pub fn top(&self) -> &Frame {
        self.stack.last().expect("stack never empty")
    }
    fn top_mut(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("stack never empty")
    }

    /// Whether the caller's role reads as read-only by name. e9s never writes either way.
    pub fn read_only_role(&self) -> bool {
        self.arn.as_ref().is_some_and(|a| {
            let role = a.to_lowercase();
            ["readonly", "read-only", "viewonly", "view-only", "securityaudit"].iter().any(|k| role.contains(k))
        })
    }

    fn identity(&mut self) {
        let (aws, tx) = (self.aws.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let result = aws.call("sts", "get-caller-identity", &[]);
            let _ = tx.send(Msg::Identity { ctx: aws.ctx.clone(), result });
        });
    }

    fn load(&mut self) {
        self.seq += 1;
        let seq = self.seq;
        let (aws, tx) = (self.aws.on_screen(self.top().id), self.tx.clone());
        let f = self.top_mut();
        f.seq = seq;
        f.loading = true;
        f.load_started = Instant::now();
        let view = f.view.clone();
        std::thread::spawn(move || {
            let result = view.fetch(&aws);
            let _ = tx.send(Msg::Loaded { seq, result });
        });
        self.next_refresh = Instant::now() + Duration::from_secs(self.interval.max(1));
    }

    fn push(&mut self, view: View) {
        self.stack.push(Frame::new(view));
        self.load();
    }

    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop().unwrap().stop_tail();
            // Screens stacked by the command line load when first reached.
            if self.top().view.auto_refresh() || !self.top().loaded {
                self.load();
            }
        }
    }

    fn replace_root(&mut self, view: View) {
        for f in &mut self.stack {
            f.stop_tail();
        }
        self.stack = vec![Frame::new(view)];
        self.load();
    }

    pub fn shutdown(&mut self) {
        for f in &mut self.stack {
            f.stop_tail();
        }
        self.ended = crate::forward::stop_all();
    }

    fn set_profile(&mut self, profile: String) {
        let region = default_region(&profile);
        self.aws = Arc::new(Aws::new(Ctx { profile, region }));
        (self.arn, self.account, self.ident_err, self.cluster, self.service) = (None, None, None, None, None);
        self.identity();
    }

    fn set_region(&mut self, region: &str) {
        self.aws = Arc::new(self.aws.in_region(region));
        (self.cluster, self.service) = (None, None);
    }

    pub fn flash(&mut self, msg: impl Into<String>, bad: bool) {
        self.flash = Some((msg.into(), bad, Instant::now()));
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Loaded { seq, result } => {
                if let Err(AwsError::SsoExpired(_)) = &result {
                    self.credentials_expired();
                } else if result.is_ok() {
                    (self.sso_expired, self.offered) = (false, false);
                }
                let Some(f) = self.stack.iter_mut().find(|f| f.seq == seq) else { return };
                f.loading = false;
                match result {
                    Ok(t) => {
                        // Back on the picker or region list: the cursor starts on the one in use.
                        let current = match f.view {
                            View::Profiles if !f.loaded => Some(self.aws.ctx.profile.clone()),
                            View::Regions if !f.loaded => Some(self.aws.ctx.region.clone()),
                            _ => None,
                        };
                        let keep = current.or_else(|| f.selected().map(|r| r.id.clone()));
                        let first = !f.loaded;
                        f.table = t;
                        f.error = None;
                        f.loaded = true;
                        let vis = f.visible();
                        // Logs open on the newest line, except head, which opens on the first.
                        f.sel = if first && matches!(&f.view, View::Logs { opts, .. } if opts.since != Since::Head) {
                            vis.len().saturating_sub(1)
                        } else {
                            keep.and_then(|id| vis.iter().position(|&i| f.table.rows[i].id == id)).unwrap_or(f.sel.min(vis.len().saturating_sub(1)))
                        };
                        // Logs follow by default, like k9s; f stops and restarts it.
                        if first && matches!(&f.view, View::Logs { opts, .. } if opts.since != Since::Head) {
                            self.toggle_tail();
                        }
                    }
                    Err(e) => f.error = Some(e),
                }
            }
            Msg::Identity { ctx, result } => {
                if ctx != self.aws.ctx {
                    return;
                }
                match result {
                    Ok(v) => {
                        self.arn = v["Arn"].as_str().map(String::from);
                        self.account = v["Account"].as_str().map(String::from);
                        self.ident_err = None;
                        (self.sso_expired, self.offered) = (false, false);
                    }
                    Err(e) => {
                        if matches!(e, AwsError::SsoExpired(_)) {
                            self.credentials_expired();
                        }
                        self.ident_err = Some(e.to_string());
                    }
                }
            }
            Msg::Tail { seq, rows, skipped } => {
                if skipped {
                    self.flash("live tail fell behind: lines arrive faster than one poll can read, some were skipped. p narrows", true);
                }
                let Some(f) = self.stack.iter_mut().find(|f| f.seq == seq) else { return };
                let at_end = f.sel + 1 >= f.visible().len();
                f.table.rows.extend(rows);
                if f.table.rows.len() > TAIL_CAP {
                    f.table.rows.drain(..f.table.rows.len() - TAIL_CAP);
                }
                if at_end {
                    f.sel = f.visible().len().saturating_sub(1);
                }
            }
            Msg::TailEnded { seq, err } => {
                if let Some(f) = self.stack.iter_mut().find(|f| f.seq == seq) {
                    if f.tail.take().is_some() {
                        let msg = if err.trim().is_empty() { "live tail ended".to_string() } else { format!("live tail ended: {}", err.trim()) };
                        self.flash(msg, true);
                    }
                }
            }
        }
    }

    pub fn tick(&mut self) {
        if self.flash.as_ref().is_some_and(|f| f.2.elapsed() > Duration::from_secs(5)) {
            self.flash = None;
        }
        let f = self.top();
        // :pf reads nothing from AWS, so its state and age follow every second.
        let pf = matches!(f.view, View::Forwards) && f.load_started.elapsed() >= Duration::from_secs(1);
        if (pf || self.interval > 0 && Instant::now() >= self.next_refresh && f.view.auto_refresh()) && !f.loading {
            self.load();
        }
    }

    pub fn refresh_in(&self) -> Option<u64> {
        (self.interval > 0 && self.top().view.auto_refresh()).then(|| self.next_refresh.saturating_duration_since(Instant::now()).as_secs())
    }

    // ---------- keys ----------

    /// Scroll keys for an overlay; None closes it.
    fn scroll(&self, k: KeyEvent, off: u16) -> Option<u16> {
        let page = self.page.max(1) as u16;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        Some(match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('H') | KeyCode::Enter => return None,
            KeyCode::Down | KeyCode::Char('j') => off.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => off.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => off.saturating_add(page),
            KeyCode::Char('d') | KeyCode::Char('f') if ctrl => off.saturating_add(page),
            KeyCode::PageUp => off.saturating_sub(page),
            KeyCode::Char('u') | KeyCode::Char('b') if ctrl => off.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => u16::MAX,
            _ => off,
        })
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Help(off) => self.mode = self.scroll(k, off).map_or(Mode::Normal, Mode::Help),
            Mode::OfferLogin(l) => match k.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('L') => self.login = Some(l),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.flash("not logged in: L logs in later", true),
                _ => self.mode = Mode::OfferLogin(l),
            },
            Mode::Calls(off) => self.mode = self.scroll(k, off).map_or(Mode::Normal, Mode::Calls),
            // k9s: Cancel has focus first, arrows or Tab move it, Enter presses it.
            Mode::Confirm(w, ok) => match k.code {
                KeyCode::Enter if ok => self.apply(w),
                KeyCode::Char('y') => self.apply(w),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.flash("nothing written", false),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Char('l') => self.mode = Mode::Confirm(w, !ok),
                _ => self.mode = Mode::Confirm(w, ok),
            },
            // Tab is text here, not completion.
            Mode::Forward(form) => self.forward_key(form, k),
            Mode::Value(s) if k.code == KeyCode::Tab => self.mode = Mode::Value(s),
            Mode::Value(s) => self.mode = self.edit(s, k, Mode::Value, |app, s| app.submit(s)),
            Mode::Normal => self.normal_key(k),
            Mode::Command(s) if matches!(k.code, KeyCode::Up | KeyCode::Down) => {
                // k9s: Up/Down walk the suggestions for what was typed.
                let (prefix, i) = self.pick.take().unwrap_or((s.clone(), usize::MAX));
                let list = completions(&prefix);
                if list.is_empty() {
                    self.mode = Mode::Command(s);
                    return;
                }
                let i = match (k.code, i) {
                    (KeyCode::Down, usize::MAX) => 0,
                    (KeyCode::Up, usize::MAX) => list.len() - 1,
                    (KeyCode::Down, i) => (i + 1) % list.len(),
                    (_, i) => (i + list.len() - 1) % list.len(),
                };
                self.mode = Mode::Command(list[i].to_string());
                self.pick = Some((prefix, i));
            }
            Mode::Command(s) => {
                self.pick = None;
                self.mode = self.edit(s, k, Mode::Command, |app, s| app.run_typed(&s))
            }
            Mode::Filter(s) => {
                self.mode = self.edit(s, k, Mode::Filter, |app, s| app.top_mut().filter = s);
                if let Mode::Filter(s) = &self.mode {
                    let s = s.clone();
                    let f = self.top_mut();
                    f.filter = s;
                    // Logs stay at the newest line, the way a followed log reads; describe jumps to the first match.
                    match f.view {
                        View::Logs { .. } => f.sel = f.visible().len().saturating_sub(1),
                        View::Detail { .. } => {
                            f.jump_match(0, true);
                        }
                        _ => f.sel = 0,
                    }
                } else if k.code == KeyCode::Esc {
                    self.top_mut().filter.clear();
                }
            }
            Mode::Pattern(s) => {
                self.mode = self.edit(s, k, Mode::Pattern, |app, pat| {
                    if let View::Logs { src, opts, .. } = app.top().view.clone() {
                        app.replace_top(View::Logs { src, pattern: pat, opts });
                    }
                })
            }
        }
    }

    /// Line editing shared by the prompts. Returns the next mode.
    fn edit(&mut self, mut s: String, k: KeyEvent, keep: fn(String) -> Mode, done: impl FnOnce(&mut App, String)) -> Mode {
        match k.code {
            KeyCode::Esc => return Mode::Normal,
            KeyCode::Enter => {
                done(self, s);
                return std::mem::replace(&mut self.mode, Mode::Normal);
            }
            KeyCode::Backspace => {
                s.pop();
            }
            KeyCode::Tab => {
                if let Some(c) = completions(&s).first() {
                    s = format!("{c} ");
                }
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => s.clear(),
            KeyCode::Char(c) => s.push(c),
            _ => {}
        }
        keep(s)
    }

    fn move_sel(&mut self, delta: isize) {
        let len = self.top().visible().len();
        let f = self.top_mut();
        f.sel = (f.sel as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
    }

    fn normal_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if !ctrl && self.log_key(k.code) {
            return;
        }
        let page = self.page.max(1) as isize;
        match k.code {
            KeyCode::Char('x') if matches!(self.top().view, View::SecretValue { .. }) => {
                if let View::SecretValue { name, reveal } = self.top().view.clone() {
                    self.replace_top(View::SecretValue { name, reveal: !reveal });
                }
            }
            KeyCode::Char('c') | KeyCode::Char('e') | KeyCode::Char('E') if !ctrl && matches!(self.top().view, View::SecretValue { .. }) => self.secret_key(k.code),
            KeyCode::Char(':') => self.mode = Mode::Command(String::new()),
            KeyCode::Char('/') => self.mode = Mode::Filter(self.top().filter.clone()),
            KeyCode::Char('?') => self.mode = Mode::Help(0),
            KeyCode::Char('H') => self.mode = Mode::Calls(0),
            KeyCode::Esc => {
                if self.top().filter.is_empty() {
                    self.pop();
                } else {
                    self.top_mut().filter.clear();
                }
            }
            KeyCode::Char('q') => {
                if self.stack.len() > 1 { self.pop() } else { self.quit = true }
            }
            KeyCode::Char('d') if ctrl && matches!(self.top().view, View::Forwards) => self.stage_stop_forward(),
            KeyCode::Char('s') | KeyCode::Char('t') if !ctrl && matches!(self.top().view, View::Forwards) => self.ask_db_user(k.code == KeyCode::Char('t')),
            KeyCode::Char('F') if matches!(self.top().view, View::Endpoints { .. }) => self.forward_form(),
            KeyCode::Char('f') if !ctrl && matches!(self.top().view, View::Endpoints { .. }) => self.show_forward(),
            KeyCode::Char('F') if matches!(self.top().view, View::Rds) => match self.top().selected().map(|r| r.cells[1].clone()) {
                Some(id) => {
                    self.push(View::Endpoints { of: Some(id) });
                    self.flash("pick the endpoint to forward (writer, reader...), then F", false);
                }
                None => self.flash("select a cluster or instance", true),
            },
            KeyCode::Char('d') | KeyCode::Char('k') if ctrl => self.flash("e9s is read-only: nothing to delete or kill", true),
            KeyCode::Char('a') if ctrl => self.push(View::Aliases),
            KeyCode::Char('e') if ctrl => self.chrome.headless = !self.chrome.headless,
            KeyCode::Char('g') if ctrl => self.chrome.crumbsless = !self.chrome.crumbsless,
            KeyCode::Char('z') if ctrl && !self.top().view.is_text() => {
                let f = self.top_mut();
                f.faults = !f.faults;
                f.sel = 0;
            }
            KeyCode::Char('u') if ctrl => self.move_sel(-page),
            KeyCode::Char('f') if ctrl => self.move_sel(page),
            KeyCode::Char('b') if ctrl => self.move_sel(-page),
            KeyCode::Char('r') if !ctrl && crate::aws::WRITE.load(Ordering::Relaxed) && matches!(self.top().view, View::Services { .. }) => self.stage_restart(),
            KeyCode::Char('r') => {
                if self.sso_expired {
                    crate::sdk::reset(); // read the login again, in case it was renewed outside e9s
                }
                self.load();
                if self.sso_expired || self.arn.is_none() {
                    self.identity();
                }
            }
            KeyCode::Char('L') => self.ask_login(),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('g') | KeyCode::Home => self.top_mut().sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.move_sel(isize::MAX / 2),
            KeyCode::Enter => self.enter(),
            KeyCode::Char('d') => self.describe(false),
            KeyCode::Char('n') | KeyCode::Char('N') if matches!(self.top().view, View::Detail { .. }) && !self.top().filter.is_empty() => {
                let fwd = k.code == KeyCode::Char('n');
                let f = self.top_mut();
                let from = if fwd { f.sel + 1 } else { f.sel + f.table.rows.len().max(1) - 1 };
                if !f.jump_match(from, fwd) {
                    self.flash("no match", true);
                }
            }
            KeyCode::Char('y') => self.describe(true),
            KeyCode::Char('l') => self.open_logs(),
            KeyCode::Char('f') if self.top().view.is_text() => {
                let f = self.top_mut();
                f.full = !f.full;
            }
            KeyCode::Char('s') if !ctrl && matches!(self.top().view, View::Logs { .. }) => self.toggle_tail(),
            // k9s details view: c copies the page.
            KeyCode::Char('c') if matches!(self.top().view, View::Detail { .. }) => {
                let rows = &self.top().table.rows;
                let text: String = rows.iter().map(|r| format!("{}\n", r.cells[0])).collect();
                match copy_to_clipboard(&text) {
                    Ok(()) => self.flash(format!("copied {} lines", rows.len()), false),
                    Err(e) => self.flash(e, true),
                }
            }
            KeyCode::Char('c') => self.copy_launch_command(),
            KeyCode::Char('t') if matches!(self.top().view, View::Logs { .. }) => {
                let f = self.top_mut();
                f.show_time = !f.show_time;
            }
            KeyCode::Char('w') => {
                let f = self.top_mut();
                f.wrap = !f.wrap;
            }
            KeyCode::Char('s') if ctrl => self.save(),
            KeyCode::Char('s') if matches!(self.top().view, View::Services { .. } | View::ScalableTargets { .. }) => self.scale(),
            KeyCode::Char('s') => self.flash("e9s is read-only: no shell (ctrl-s saves this view)", true),
            KeyCode::Char('e') => match self.service_of_view() {
                Some((cluster, service)) => self.push(View::Events { cluster, service }),
                None => self.flash("e works on a service, or a task or failure of one", true),
            },
            KeyCode::Char('m') => match self.top().view.metrics(self.top().selected()) {
                Some(target) => self.push(View::Metrics { target }),
                None => self.flash("no metrics for this view", true),
            },
            KeyCode::Char('T') => match self.top().selected().and_then(|r| self.top().view.trail_name(r)) {
                Some(name) => self.push(View::Trail { resource: Some(name) }),
                None => self.flash("T (who changed it) works on RDS, Lambda and EC2 rows; :trail lists every change", true),
            },
            KeyCode::Char('p') => match (&self.top().view, self.top().selected()) {
                (View::Logs { pattern, .. }, _) => self.mode = Mode::Pattern(pattern.clone()),
                (View::ScalableTargets { ns }, Some(r)) => {
                    let v = View::ScalingPolicies { ns: ns.clone(), resource: Some(r.id.clone()) };
                    self.push(v)
                }
                _ => self.flash("p works in log and scaling views", true),
            },
            KeyCode::Char('F') => match self.service_of_view() {
                Some((cluster, service)) => self.push(View::Failures { cluster, service: Some(service) }),
                None => match self.cluster.clone() {
                    Some(cluster) => self.push(View::Failures { cluster, service: None }),
                    None => self.flash("F works on a service or inside a cluster", true),
                },
            },
            KeyCode::Char('R') => match self.service_of_view() {
                Some((cluster, service)) => {
                    self.service = Some(service.clone());
                    self.push(View::Rollout { cluster, service })
                }
                None => self.flash("R works on a service, or a task or failure of one", true),
            },
            KeyCode::Char('+') | KeyCode::Char('=') => self.step_interval(),
            KeyCode::Char('[') => self.history_go(self.hist_at.checked_sub(1)),
            KeyCode::Char(']') => self.history_go(Some(self.hist_at + 1)),
            KeyCode::Char('-') => self.history_go(Some(self.hist_last)),
            KeyCode::Char('<') => self.sort_step(-1),
            KeyCode::Char('>') => self.sort_step(1),
            KeyCode::Char('o') => {
                let f = self.top_mut();
                f.sort = Some(f.sort.map_or((0, true), |(c, d)| (c, !d)));
            }
            KeyCode::Char('N') => self.sort_by(0),
            KeyCode::Char('S') => match self.top().table.headers.iter().position(|h| ["STATUS", "LAST", "STATE"].contains(&h.as_str())) {
                Some(c) => self.sort_by(c),
                None => self.flash("no status column here", true),
            },
            KeyCode::Char('A') => match self.top().table.headers.iter().position(|h| h.contains("AGE")) {
                Some(c) => self.sort_by(c),
                None => self.flash("no AGE column here", true),
            },
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some((_, cmd)) = QUICK.iter().find(|(k, _)| *k == c) {
                    self.exec(cmd);
                }
            }
            _ => {}
        }
    }

    /// `c` `e` `E` on a secret: read it fresh (the screen may be minutes old), then copy, edit a key, or hand it to $EDITOR.
    fn secret_key(&mut self, code: KeyCode) {
        let View::SecretValue { name, .. } = self.top().view.clone() else { return };
        let key = self.top().selected().and_then(|r| r.json["key"].as_str().map(String::from));
        let sec = match read_secret(&self.aws.on_screen(self.top().id), &name) {
            Ok(s) => s,
            Err(e) => return self.flash(e.to_string(), true),
        };
        let value = match &key {
            Some(k) => secret_keys(&sec.text).and_then(|kv| kv.into_iter().find(|(x, _)| x == k)).map(|(_, v)| v).unwrap_or_default(),
            None => sec.text.clone(),
        };
        match code {
            KeyCode::Char('c') => match copy_to_clipboard(&value) {
                Ok(()) => self.flash(format!("copied the value of {}", key.as_deref().unwrap_or(&name)), false),
                Err(e) => self.flash(e, true),
            },
            KeyCode::Char('e') => {
                self.editing = Some(Editing::SecretKey(sec, key));
                self.mode = Mode::Value(value);
            }
            _ => self.editor = Some(sec),
        }
    }

    /// `s`: the desired count of a service, or the range of a scaling target.
    fn scale(&mut self) {
        if !crate::aws::WRITE.load(Ordering::Relaxed) {
            return self.flash("s changes scaling only when e9s is started with --write", true);
        }
        let Some(row) = self.top().selected().cloned() else { return };
        let num = |p: &str| crate::logic::n(&row.json, p);
        if let View::Services { cluster } = self.top().view.clone() {
            let now = num("/desiredCount");
            self.mode = Mode::Value(now.to_string());
            self.editing = Some(Editing::Desired { cluster, service: row.id.clone(), now });
        } else {
            self.mode = Mode::Value(format!("{} {}", num("/MinCapacity"), num("/MaxCapacity")));
            self.editing = Some(Editing::Range(row));
        }
    }

    /// Enter in the value prompt: stage what was typed for y.
    fn submit(&mut self, input: String) {
        match self.editing.take() {
            Some(Editing::SecretKey(sec, key)) => {
                let text = set_key(&sec.text, key.as_deref(), &input);
                self.stage_secret(sec, text);
            }
            Some(Editing::Range(row)) => self.stage_range(&row, &input),
            Some(Editing::Desired { cluster, service, now }) => self.stage_desired(cluster, service, now, &input),
            Some(Editing::DbUser { session, copy }) => self.db_login(&session, copy, &input),
            None => {}
        }
    }

    /// After $EDITOR: JSON goes back compact, as the console stores it; the newline an editor adds is dropped.
    pub fn edited(&mut self, sec: Secret, result: Result<String, String>) {
        match result {
            Ok(text) => {
                let text = match serde_json::from_str::<Value>(&text) {
                    Ok(v @ Value::Object(_)) => v.to_string(),
                    _ if !sec.text.ends_with('\n') => text.strip_suffix('\n').unwrap_or(&text).to_string(),
                    _ => text,
                };
                self.stage_secret(sec, text);
            }
            Err(e) => self.flash(format!("editor: {e}; nothing written"), true),
        }
    }

    fn stage_secret(&mut self, read: Secret, text: String) {
        let changes = secret_change(&read.text, &text);
        if changes.is_empty() {
            return self.flash("no change, nothing written", false);
        }
        self.mode = Mode::Confirm(Pending {
            title: format!("Save a new version of {}?", read.name),
            changes,
            note: "The current value stays as AWSPREVIOUS. Refused if the secret changed since e9s read it.".into(),
            write: Write::Secret { read, text },
            then: None,
        }, false);
    }

    fn stage_range(&mut self, row: &Row, input: &str) {
        let (min, max, secs) = match parse_scale(input) {
            Ok(x) => x,
            Err(e) => return self.flash(e, true),
        };
        let g = |p: &str| crate::logic::s(&row.json, p);
        let (old_min, old_max) = (crate::logic::n(&row.json, "/MinCapacity"), crate::logic::n(&row.json, "/MaxCapacity"));
        let (ns, id, dim) = (g("/ServiceNamespace"), g("/ResourceId"), g("/ScalableDimension"));
        let mut changes: Vec<String> = [("min", old_min, min), ("max", old_max, max)].iter().filter(|(_, a, b)| a != b).map(|(k, a, b)| format!("{k} {a} -> {b}")).collect();
        if changes.is_empty() && secs.is_none() {
            return self.flash("no change, nothing written", false);
        }
        let target = |extra: &[String]| -> Vec<String> {
            let mut a: Vec<String> = ["--service-namespace", &ns, "--resource-id", &id, "--scalable-dimension", &dim].map(String::from).to_vec();
            a.extend_from_slice(extra);
            a
        };
        let mut calls = vec![("application-autoscaling", "register-scalable-target", target(&["--min-capacity".into(), min.to_string(), "--max-capacity".into(), max.to_string()]))];
        let mut note = "Autoscaling moves the count into the new range right away.".to_string();
        match (secs, row.cells.get(5).filter(|c| !c.is_empty())) {
            (Some(secs), _) => {
                let at = (chrono::Utc::now() + chrono::Duration::seconds(secs)).format("%Y-%m-%dT%H:%M:%S").to_string();
                changes.push(format!("back to {old_min}-{old_max} at {}", local_hm(&at)));
                note += &format!(" The way back is an Application Auto Scaling scheduled action ({RESTORE_ACTION}), so it runs without e9s; RESTORE shows it.");
                calls.push(("application-autoscaling", "put-scheduled-action", target(&[
                    "--scheduled-action-name".into(), RESTORE_ACTION.into(), "--schedule".into(), format!("at({at})"),
                    "--scalable-target-action".into(), format!("MinCapacity={old_min},MaxCapacity={old_max}"),
                ])));
            }
            (None, Some(pending)) => note += &format!(" The restore already scheduled ({pending}) still runs."),
            _ => {}
        }
        self.mode = Mode::Confirm(Pending { title: format!("Change the scaling range of {}?", id.trim_start_matches("service/")), changes, note, write: Write::Calls(calls), then: None }, false);
    }

    fn stage_desired(&mut self, cluster: String, service: String, now: i64, input: &str) {
        let Some(count) = input.trim().parse::<i64>().ok().filter(|c| *c >= 0) else { return self.flash("type a task count, like 4", true) };
        if count == now {
            return self.flash("no change, nothing written", false);
        }
        // Autoscaling, if the service has it, can undo the change: say so before y.
        let aws = self.aws.on_screen(self.top().id);
        let id = format!("service/{cluster}/{service}");
        let target = aws.call("application-autoscaling", "describe-scalable-targets", &["--service-namespace".into(), "ecs".into(), "--resource-ids".into(), id.clone()]);
        let mine = |v: &Value| v["ScalableTargets"].as_array().and_then(|ts| ts.iter().find(|t| t["ResourceId"] == id.as_str()).cloned());
        let note = match target.as_ref().map(mine) {
            Ok(Some(t)) => {
                let (lo, hi) = (crate::logic::n(&t, "/MinCapacity"), crate::logic::n(&t, "/MaxCapacity"));
                if count < lo || count > hi {
                    format!("Autoscaling keeps it between {lo} and {hi}, so it goes back into that range on its next scaling action. s in :scaling changes the range.")
                } else {
                    format!("Autoscaling (min {lo}, max {hi}) can change it again on its next scaling action.")
                }
            }
            Ok(None) => "No autoscaling on this service: the count stays until someone changes it.".into(),
            Err(e) => format!("Its autoscaling could not be read ({e}); autoscaling may change the count again."),
        };
        let args = ["--cluster", &cluster, "--service", &service, "--desired-count", &count.to_string()].map(String::from).to_vec();
        self.mode = Mode::Confirm(Pending { title: format!("Change the desired count of {service}?"), changes: vec![format!("desired {now} -> {count}")], note, write: Write::Calls(vec![("ecs", "update-service", args)]), then: None }, false);
    }

    /// `r` on a service (--write): a new deployment of the same task definition, so tasks start
    /// again and read secrets and parameters afresh. k9s `r` on a deployment.
    fn stage_restart(&mut self) {
        let View::Services { cluster } = self.top().view.clone() else { return };
        let Some(row) = self.top().selected().cloned() else { return };
        let j = &row.json;
        let (desired, td) = (crate::logic::n(j, "/desiredCount"), crate::logic::s(j, "/taskDefinition"));
        let dc = &j["deploymentConfiguration"];
        let (lo, hi) = (crate::logic::s(dc, "/minimumHealthyPercent"), crate::logic::s(dc, "/maximumPercent"));
        let service = row.id.clone();
        let args = ["--cluster", &cluster, "--service", &service, "--force-new-deployment"].map(String::from).to_vec();
        self.mode = Mode::Confirm(Pending {
            title: format!("Restart {service}?"),
            changes: vec![format!("new deployment: {desired} task(s) replaced, same task definition ({})", crate::logic::taskdef(&td))],
            note: format!(
                "ECS starts new tasks and stops the old ones as the new ones pass health checks (keeps at least {lo}% running, at most {hi}%). New tasks read secrets when they start, so a changed secret takes effect. The rollout watch opens next."
            ),
            write: Write::Calls(vec![("ecs", "update-service", args)]),
            then: Some(View::Rollout { cluster, service }),
        }, false);
    }

    /// `F` on an endpoint: the dialog, with the online SSM instances (likely bastions first, the last one used
    /// in this profile and region picked) and a free local port (the database's own when free).
    fn forward_form(&mut self) {
        let Some(row) = self.top().selected().cloned() else { return };
        if std::env::var_os("E9S_DEMO").is_none() && crate::forward::plugin().is_none() {
            return self.flash(crate::forward::PLUGIN_HELP, true);
        }
        let j = &row.json;
        let g = |p: &str| crate::logic::s(j, p);
        let port = crate::logic::n(j, "/Port") as u16;
        let target = crate::forward::Target {
            name: format!("{} {}", g("/Database"), g("/Endpoint")),
            host: g("/Host"),
            port,
            db: g("/DatabaseName"),
            user: g("/MasterUsername"),
            iam: j["IAMDatabaseAuthenticationEnabled"].as_bool() == Some(true),
        };
        let table = match crate::views::bastions(&self.aws.on_screen(self.top().id)) {
            Ok(t) => t,
            Err(e) => return self.flash(format!("SSM instances not loaded: {e}"), true),
        };
        let bastions: Vec<(String, String, bool)> = table
            .rows
            .iter()
            // Online, or unknown when the role cannot list SSM's instances.
            .filter(|r| r.health != Health::Bad)
            .map(|r| (r.id.clone(), r.cells[0].clone(), r.json["LikelyBastion"] == true))
            .collect();
        if bastions.is_empty() {
            return self.flash("no online SSM-managed instance to forward through in this region (:ssm lists them, and why)", true);
        }
        let last = crate::forward::recall(&crate::forward::bastion_key(&self.aws.ctx));
        let pick = last.and_then(|l| bastions.iter().position(|b| b.0 == l)).unwrap_or(0);
        let local = crate::forward::free_port(port).to_string();
        self.mode = Mode::Forward(ForwardForm { target, bastions, pick, local, fresh: true });
    }

    /// `f` on an endpoint, k9s "Show PortForward": `:pf` filtered to its forwards.
    fn show_forward(&mut self) {
        let Some(host) = self.top().selected().map(|r| crate::logic::s(&r.json, "/Host")) else { return };
        if !crate::forward::list().iter().any(|f| f["Host"] == host.as_str()) {
            return self.flash("no port-forward defined", true);
        }
        self.push(View::Forwards);
        self.top_mut().filter = host;
    }

    fn forward_key(&mut self, mut form: ForwardForm, k: KeyEvent) {
        let n = form.bastions.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return self.flash("no forward started", false),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => form.pick = (form.pick + 1) % n,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => form.pick = (form.pick + n - 1) % n,
            KeyCode::Char(c) if c.is_ascii_digit() && (form.fresh || form.local.len() < 5) => {
                if std::mem::take(&mut form.fresh) {
                    form.local.clear();
                }
                form.local.push(c)
            }
            KeyCode::Backspace => {
                form.fresh = false;
                form.local.pop();
            }
            KeyCode::Enter => return self.start_forward(form),
            _ => {}
        }
        self.mode = Mode::Forward(form);
    }

    fn start_forward(&mut self, form: ForwardForm) {
        let Some(local) = form.local.parse::<u16>().ok().filter(|p| *p > 0) else {
            self.flash("type a local port, like 15432", true);
            self.mode = Mode::Forward(form);
            return;
        };
        let (id, name, _) = form.bastions[form.pick].clone();
        let aws = self.aws.on_screen(self.top().id);
        match crate::forward::start(&aws, (id.clone(), name.clone()), form.target.clone(), local) {
            Ok(_) => {
                crate::forward::remember(&crate::forward::bastion_key(&self.aws.ctx), &id);
                self.flash(format!("forwarding localhost:{local} -> {}:{} via {name}", form.target.host, form.target.port), false);
                self.push(View::Forwards);
            }
            Err(e) => {
                self.flash(e, true);
                self.mode = Mode::Forward(form);
            }
        }
    }

    /// ctrl-d on `:pf`, k9s delete: asks first, Cancel focused.
    fn stage_stop_forward(&mut self) {
        let Some(r) = self.top().selected().cloned() else { return };
        let g = |p: &str| crate::logic::s(&r.json, p);
        self.mode = Mode::Confirm(Pending {
            title: format!("Stop the port forward on localhost:{}?", g("/LocalPort")),
            changes: vec![format!("localhost:{} -> {}:{} via {}", g("/LocalPort"), g("/Host"), g("/Port"), g("/BastionName"))],
            note: "Ends its Session Manager session (ssm terminate-session); open connections through it close.".into(),
            write: Write::StopForward(r.id.clone()),
            then: None,
        }, false);
    }

    /// `s` / `t` on `:pf`: ask for the db user and database, the last ones used for that host first.
    fn ask_db_user(&mut self, copy: bool) {
        let Some(r) = self.top().selected().cloned() else { return };
        let Some((_, t, _)) = crate::forward::get(&r.id) else { return self.flash("that forward is gone", true) };
        if copy && !t.iam {
            return self.flash("IAM database authentication is off for this database: there is no token to make", true);
        }
        let last = crate::forward::recall(&format!("dbuser {}", t.host));
        self.editing = Some(Editing::DbUser { session: r.id.clone(), copy });
        self.mode = Mode::Value(last.unwrap_or_else(|| format!("{} {}", t.user, t.db).trim().to_string()));
    }

    /// The token is a local signature (`rds generate-db-auth-token`, in `H`), good for 15 minutes; psql gets it as PGPASSWORD.
    fn db_login(&mut self, session: &str, copy: bool, input: &str) {
        let Some((ctx, t, local)) = crate::forward::get(session) else { return self.flash("that forward is gone", true) };
        let mut w = input.split_whitespace();
        let Some(user) = w.next().map(String::from) else { return self.flash("type a database user, and optionally a database name", true) };
        let db = w.next().map(String::from);
        crate::forward::remember(&format!("dbuser {}", t.host), input.trim());
        let token = if t.iam {
            let args = ["--hostname", &t.host, "--port", &t.port.to_string(), "--username", &user].map(String::from);
            match Aws::new(ctx).on_screen(self.top().id).call("rds", "generate-db-auth-token", &args) {
                Ok(v) => v["Token"].as_str().map(String::from),
                Err(e) => return self.flash(e.to_string(), true),
            }
        } else {
            None
        };
        if copy {
            return match copy_to_clipboard(&token.unwrap_or_default()) {
                Ok(()) => self.flash(format!("copied an IAM auth token for {user} (valid 15 minutes): psql -h localhost -p {local} -U {user} sslmode=require"), false),
                Err(e) => self.flash(e, true),
            };
        }
        let mut env = vec![("PGHOST".to_string(), "localhost".to_string()), ("PGPORT".into(), local.to_string()), ("PGUSER".into(), user.clone())];
        if let Some(db) = &db {
            env.push(("PGDATABASE".into(), db.clone()));
        }
        if let Some(tok) = token {
            // IAM auth needs TLS; the certificate names the RDS host, not localhost, so it is not verified.
            env.extend([("PGPASSWORD".into(), tok), ("PGSSLMODE".into(), "require".into())]);
        }
        self.psql = Some(Psql { env, what: format!("psql -h localhost -p {local} -U {user}{}", db.map(|d| format!(" -d {d}")).unwrap_or_default()) });
    }

    /// After psql exits.
    pub fn psql_done(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.flash("psql exited", false),
            Err(e) => self.flash(e, true),
        }
    }

    /// y: do the staged write. A secret is read again first and left alone if it changed since.
    fn apply(&mut self, p: Pending) {
        let aws = self.aws.on_screen(self.top().id);
        let verb = if matches!(p.write, Write::StopForward(_)) { "stopped" } else { "done" };
        let r = match p.write {
            Write::StopForward(session) => {
                let ctx = crate::forward::get(&session).map_or(self.aws.ctx.clone(), |f| f.0);
                crate::forward::stop(&Aws::new(ctx).on_screen(self.top().id), &session).map_err(AwsError::Api)
            }
            Write::Secret { read, text } => read_secret(&aws, &read.name).and_then(|now| {
                if now.version != read.version {
                    return Err(AwsError::Api(format!("{} changed since e9s read it: nothing written, r reads it again", read.name)));
                }
                aws.call("secretsmanager", "put-secret-value", &["--secret-id".into(), read.name.clone(), "--secret-string".into(), text]).map(|_| ())
            }),
            Write::Calls(calls) => {
                let mut done = vec![];
                calls.iter().try_for_each(|(svc, op, args)| aws.call(svc, op, args).map(|_| done.push(*op))).map_err(|e| match done.is_empty() {
                    true => e,
                    false => AwsError::Api(format!("{e} · already done: {}", done.join(", "))),
                })
            }
        };
        match r {
            Ok(()) => {
                self.flash(format!("{verb}: {}", p.changes.join(", ")), false);
                match p.then {
                    Some(v) => self.push(v),
                    None => self.load(),
                }
            }
            Err(e) => self.flash(e.to_string(), true),
        }
    }

    /// What logs `profile` in: itself (SSO, `aws login`) or, for a role, the profile it signs in through.
    pub fn login_for(profile: &str) -> Option<Login> {
        let all = crate::aws::profile_details();
        crate::aws::login_target(profile, &all).map(|(p, auth)| Login { args: crate::aws::login_args(&p, &auth), profile: p })
    }

    /// `L`: the selected profile in the picker, else the current one.
    fn ask_login(&mut self) {
        let profile = match (&self.top().view, self.top().selected()) {
            (View::Profiles, Some(r)) => r.id.clone(),
            _ => self.aws.ctx.profile.clone(),
        };
        match App::login_for(&profile) {
            Some(l) => self.login = Some(l),
            None => {
                let who = if profile.is_empty() { "the default credentials".to_string() } else { format!("profile {profile}") };
                self.flash(format!("{who} has no login to run (keys, credential_process or an MFA role): renew its credentials, then r"), true);
            }
        }
    }

    /// Credentials expired or are missing. The first time, if the profile has a login, offer to run it.
    fn credentials_expired(&mut self) {
        self.sso_expired = true;
        if self.offered || self.login.is_some() {
            return;
        }
        self.offered = true;
        self.expired_login = App::login_for(&self.aws.ctx.profile);
        if let (Some(l), Mode::Normal) = (&self.expired_login, &self.mode) {
            self.mode = Mode::OfferLogin(l.clone());
        }
    }

    /// After the login command ran: read credentials again, re-check identity, reload the screen.
    pub fn logged_in(&mut self, login: &Login, result: Result<(), String>) {
        match result {
            Ok(()) => {
                crate::sdk::reset();
                self.flash(format!("logged in: {}", login.profile), false);
                // No second offer if it still fails: the header keeps the L hint.
                self.offered = true;
                self.identity();
                self.load();
            }
            Err(e) => self.flash(format!("aws {} failed: {e}", login.args.join(" ")), true),
        }
    }

    /// The service the current view is about: the selected row in services (or the service of the
    /// selected task or failure, across a cluster), else the view's own service.
    fn service_of_view(&self) -> Option<(String, String)> {
        match (&self.top().view, self.top().selected()) {
            (View::Services { cluster }, Some(r)) => Some((cluster.clone(), r.id.clone())),
            (View::Tasks { cluster, service: None }, Some(r)) => {
                r.json["group"].as_str().and_then(|g| g.strip_prefix("service:")).map(|s| (cluster.clone(), s.to_string()))
            }
            (View::Failures { cluster, .. }, Some(r)) => Some((cluster.clone(), r.cells[1].clone())),
            (View::Tasks { cluster, service: Some(sv) } | View::Events { cluster, service: sv } | View::Rollout { cluster, service: sv }, _) => {
                Some((cluster.clone(), sv.clone()))
            }
            _ => None,
        }
    }

    /// The command line that reopens this view, or the nearest view above it that the CLI can open.
    pub fn launch_command(&self) -> (String, bool) {
        let bin = std::env::args().next().map(|a| a.rsplit('/').next().unwrap_or(&a).to_string()).unwrap_or_else(|| "e9s".into());
        let q = |v: &str| if v.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c)) { v.to_string() } else { format!("'{}'", v.replace('\'', "'\\''")) };
        let ctx = &self.aws.ctx;
        for (depth, fr) in self.stack.iter().rev().enumerate() {
            let svc = |c: &str, s: &str, cmd: Option<&str>| (Some(c.to_string()), Some(s.to_string()), cmd.map(String::from));
            let (cluster, service, command): (Option<String>, Option<String>, Option<String>) = match &fr.view {
                View::Profiles => (None, None, Some("profile".into())),
                View::Regions => (None, None, Some("region".into())),
                View::Clusters => (None, None, None),
                View::Services { cluster } => (Some(cluster.clone()), None, None),
                View::Tasks { cluster, service: Some(s) } => svc(cluster, s, None),
                View::Tasks { cluster, service: None } => (Some(cluster.clone()), None, Some("tasks".into())),
                View::Events { cluster, service } => svc(cluster, service, Some("events")),
                View::Rollout { cluster, service } => svc(cluster, service, Some("rollout")),
                View::Failures { cluster, service: Some(s) } => svc(cluster, s, Some("failures")),
                View::Failures { cluster, service: None } => (Some(cluster.clone()), None, Some("failures".into())),
                View::TaskDefFamilies => (None, None, Some("taskdefs".into())),
                View::LogGroups => (None, None, Some("logs".into())),
                View::Logs { src: LogSrc::Group(g), pattern, .. } => (None, None, Some(format!("logs {g} {pattern}").trim_end().to_string())),
                View::LoadBalancers => (None, None, Some("lb".into())),
                View::TargetGroups { lb: None } => (None, None, Some("tg".into())),
                View::ScalableTargets { ns } => (None, None, Some(format!("scaling {ns}"))),
                View::ScalingPolicies { ns, resource: None } => (None, None, Some(format!("policies {ns}"))),
                View::ScalingActivities { ns, resource: None } => (None, None, Some(format!("activities {ns}"))),
                View::Ec2 => (None, None, Some("ec2".into())),
                View::EcrRepos => (None, None, Some("ecr".into())),
                View::Sqs => (None, None, Some("sqs".into())),
                View::Rds => (None, None, Some("rds".into())),
                View::Endpoints { of } => (None, None, Some(format!("endpoints {}", of.as_deref().unwrap_or_default()).trim_end().to_string())),
                View::Bastions => (None, None, Some("ssm".into())),
                View::Lambda => (None, None, Some("lambda".into())),
                View::Pulse => (None, None, Some("pulse".into())),
                View::Alarms => (None, None, Some("alarms".into())),
                View::Trail { resource } => (None, None, Some(format!("trail {}", resource.as_deref().unwrap_or_default()).trim_end().to_string())),
                View::Access => (None, None, Some("access".into())),
                View::Secrets => (None, None, Some("secrets".into())),
                View::SecretValue { name, .. } => (None, None, Some(format!("secrets {name}"))),
                _ => continue,
            };
            let mut parts = vec![bin.clone()];
            if !ctx.profile.is_empty() {
                parts.extend(["-p".into(), q(&ctx.profile)]);
            }
            if !matches!(fr.view, View::Profiles | View::Regions) {
                parts.extend(["-r".into(), q(&ctx.region)]);
            }
            if let Some(c) = cluster {
                parts.extend(["--cluster".into(), q(&c)]);
            }
            if let Some(s) = service {
                parts.extend(["-s".into(), q(&s)]);
            }
            if let Some(c) = command {
                parts.extend(["-c".into(), q(&c)]);
            }
            return (parts.join(" "), depth == 0);
        }
        (bin, false)
    }

    fn copy_launch_command(&mut self) {
        let (cmd, exact) = self.launch_command();
        let note = if exact { String::new() } else { " (nearest view the command line can open)".into() };
        match copy_to_clipboard(&cmd) {
            Ok(()) => self.flash(format!("copied: {cmd}{note}"), false),
            Err(e) => self.flash(format!("{cmd}{note}  (clipboard failed: {e})"), true),
        }
    }

    fn step_interval(&mut self) {
        let i = INTERVALS.iter().position(|&x| x == self.interval).unwrap_or(1);
        self.interval = INTERVALS[(i + 1) % INTERVALS.len()];
        self.next_refresh = Instant::now() + Duration::from_secs(self.interval);
        let msg = if self.interval == 0 { "auto-refresh off".into() } else { format!("auto-refresh every {}s", self.interval) };
        self.flash(msg, false);
    }

    fn sort_by(&mut self, c: usize) {
        let f = self.top_mut();
        f.sort = Some(match f.sort {
            Some((cc, d)) if cc == c => (c, !d),
            _ => (c, false),
        });
    }

    fn sort_step(&mut self, d: isize) {
        let n = self.top().table.headers.len().max(1) as isize;
        let f = self.top_mut();
        let (c, desc) = f.sort.unwrap_or((if d > 0 { n as usize - 1 } else { 0 }, false));
        f.sort = Some((((c as isize + d).rem_euclid(n)) as usize, desc));
    }

    fn enter(&mut self) {
        let Some(row) = self.top().selected().cloned() else { return };
        match &self.top().view {
            View::Profiles => {
                self.set_profile(row.id.clone());
                return self.push(View::Regions);
            }
            View::Regions => {
                self.set_region(&row.id);
                return self.push(View::Clusters);
            }
            View::Aliases => {
                self.pop();
                return self.exec(&row.id);
            }
            View::Clusters => self.cluster = Some(row.id.clone()),
            View::Services { .. } => self.service = Some(row.id.clone()),
            _ => {}
        }
        if let Some(v) = self.top().view.drill(&row) {
            self.push(v);
        }
    }

    fn describe(&mut self, raw: bool) {
        let f = self.top();
        if matches!(f.view, View::Detail { .. }) {
            return;
        }
        let Some(row) = f.selected() else { return };
        // RDS and Lambda: a live page that refetches this one resource, to follow a change as it applies.
        if !raw && matches!(f.view, View::Rds | View::Lambda) {
            let (kind, id) = match f.view {
                View::Rds => (row.cells[0].clone(), row.cells[1].clone()),
                _ => ("lambda".into(), row.cells[0].clone()),
            };
            let lines = if kind == "lambda" { crate::describe::lambda(&row.json) } else { vec![] };
            self.push(View::Detail { title: format!("describe({id})"), lines, live: Some((kind, id)) });
            return;
        }
        // Structured log lines: describe the parsed message.
        let doc = match (&f.view, row.json.get("message").and_then(Value::as_str)) {
            (View::Logs { .. }, Some(m)) => serde_json::from_str::<Value>(m).unwrap_or_else(|_| row.json.clone()),
            _ if row.json.is_null() => {
                Value::Object(f.table.headers.iter().cloned().zip(row.cells.iter().map(|c| Value::String(c.clone()))).collect())
            }
            _ => row.json.clone(),
        };
        let lines = if raw {
            to_yaml(&doc)
        } else {
            crate::describe::describe(&f.view, row).unwrap_or_else(|| to_yaml(&doc))
        };
        // Name the page after what the row shows first (a name), not the ID buried in its ARN.
        let name = row.cells.first().filter(|c| !c.is_empty()).cloned().unwrap_or_else(|| short(&row.id).to_string());
        self.push(View::Detail { title: format!("{}({name})", if raw { "yaml" } else { "describe" }), lines, live: None });
    }

    fn open_logs(&mut self) {
        let f = self.top();
        match f.selected().and_then(|r| f.view.log_src(r)) {
            Some(src) => self.push(View::Logs { src, pattern: String::new(), opts: LogOpts::default() }),
            None => self.flash("no logs for this selection", true),
        }
    }

    /// k9s's log-view keys: 0-6 where to start, a all containers, m mark, C clear, c copy.
    fn log_key(&mut self, code: KeyCode) -> bool {
        let View::Logs { src, pattern, opts } = self.top().view.clone() else { return false };
        let KeyCode::Char(c) = code else { return false };
        let since = match c {
            '0' => Since::Tail,
            '1' => Since::Head,
            '2' => Since::Secs(60),
            '3' => Since::Secs(300),
            '4' => Since::Secs(900),
            '5' => Since::Secs(1800),
            '6' => Since::Secs(3600),
            'a' if matches!(src, LogSrc::Service { .. }) => {
                self.replace_top(View::Logs { src, pattern, opts: LogOpts { all: !opts.all, ..opts } });
                return true;
            }
            'm' => {
                let f = self.top_mut();
                let at_end = f.sel + 1 >= f.visible().len();
                let id = format!("mark-{}", f.table.rows.len());
                let now = chrono::Local::now().format("%H:%M:%S%.3f").to_string();
                f.table.rows.push(Row { id, cells: vec![now, String::new(), "-".repeat(40)], health: crate::logic::Health::None, json: Value::String(MARK.into()) });
                if at_end {
                    f.sel = f.visible().len().saturating_sub(1);
                }
                return true;
            }
            'C' => {
                let f = self.top_mut();
                f.table.rows.clear();
                (f.sel, f.offset) = (0, 0);
                return true;
            }
            'c' => {
                let (text, n) = log_text(self.top());
                match copy_to_clipboard(&text) {
                    Ok(()) => self.flash(format!("copied {n} lines"), false),
                    Err(e) => self.flash(e, true),
                }
                return true;
            }
            _ => return false,
        };
        self.replace_top(View::Logs { src, pattern, opts: LogOpts { since, ..opts } });
        true
    }

    /// Swaps the top view for another, stopping its tail (a log view reopened with new options).
    fn replace_top(&mut self, view: View) {
        self.top_mut().stop_tail();
        self.stack.pop();
        self.push(view);
    }

    fn toggle_tail(&mut self) {
        let aws = self.aws.on_screen(self.top().id);
        let tx = self.tx.clone();
        let f = self.top_mut();
        if f.tail.is_some() {
            f.stop_tail();
            self.flash("live tail stopped (s resumes)", false);
            return;
        }
        let Some(sel) = f.table.tail.clone() else {
            self.flash("live tail needs a loaded log view", true);
            return;
        };
        // Resume after the newest line on screen, skipping lines already shown.
        let mut seen: HashMap<String, i64> =
            f.table.rows.iter().map(|r| (r.id.clone(), r.json["timestamp"].as_i64().unwrap_or(0))).collect();
        let mut cursor = seen.values().copied().max().unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let run = Arc::new(AtomicBool::new(true));
        let (seq, alive) = (f.seq, run.clone());
        std::thread::spawn(move || {
            while alive.load(Ordering::Relaxed) {
                let started = Instant::now();
                match tail_poll(&aws, &sel, &mut cursor, &mut seen) {
                    Ok((rows, skipped)) if alive.load(Ordering::Relaxed) => {
                        if tx.send(Msg::Tail { seq, rows, skipped }).is_err() {
                            return;
                        }
                    }
                    Ok(_) => return,
                    Err(e) => {
                        let _ = tx.send(Msg::TailEnded { seq, err: e.to_string() });
                        return;
                    }
                }
                while alive.load(Ordering::Relaxed) && started.elapsed() < TAIL_EVERY {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        f.tail = Some(run);
        f.sel = f.visible().len().saturating_sub(1);
    }

    /// Saves what is on screen (after filter and sort) to ~/Downloads. Logs, tailed lines included,
    /// go out as a plain .log with full timestamps; tables as tab-separated text.
    fn save(&mut self) {
        let f = self.top();
        let is_logs = matches!(f.view, View::Logs { .. });
        let (out, n) = if is_logs {
            log_text(f)
        } else {
            let mut out = String::new();
            if !f.view.is_text() {
                out.push_str(&f.table.headers.join("\t"));
                out.push('\n');
            }
            let vis = f.visible();
            for &i in &vis {
                out.push_str(&f.table.rows[i].cells.join("\t"));
                out.push('\n');
            }
            (out, vis.len())
        };
        let slug: String = f.view.title().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let dir = format!("{}/Downloads", std::env::var("HOME").unwrap_or_default());
        let ext = if is_logs { "log" } else { "txt" };
        let path = format!("{dir}/e9s-{}-{}.{ext}", slug.trim_matches('-'), chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let what = if is_logs { format!("{n} lines") } else { format!("{n} rows") };
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, out)) {
            Ok(()) => self.flash(format!("saved {what} to {path}"), false),
            Err(e) => self.flash(format!("save failed: {e}"), true),
        }
    }

    /// A command typed after `:`: run it, and remember it if it names a view.
    fn run_typed(&mut self, line: &str) {
        let line = line.trim();
        if line.split_whitespace().next().and_then(canonical).is_some_and(|c| c != "quit") && self.history.last().is_none_or(|l| l != line) {
            self.history.push(line.to_string());
            self.hist_last = self.hist_at;
            self.hist_at = self.history.len() - 1;
        }
        self.exec(line);
    }

    /// k9s `[` `]` `-`: rerun a remembered command without adding it again.
    fn history_go(&mut self, to: Option<usize>) {
        let Some(line) = to.filter(|&i| i != self.hist_at).and_then(|i| self.history.get(i)).cloned() else {
            return self.flash("no more command history", true);
        };
        self.hist_last = self.hist_at;
        self.hist_at = to.unwrap();
        self.exec(&line);
    }

    pub fn exec(&mut self, line: &str) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = words.first() else { return };
        let arg = words.get(1).map(|s| s.to_string());
        let canon = canonical(cmd);
        let need_cluster = |app: &mut App| -> Option<String> {
            let c = arg.clone().or_else(|| app.cluster.clone());
            if c.is_none() {
                app.replace_root(View::Clusters);
                app.flash("pick a cluster first (Enter)", true);
            }
            c
        };
        match canon {
            Some("clusters") => self.replace_root(View::Clusters),
            Some("services") => {
                if let Some(cluster) = need_cluster(self) {
                    self.cluster = Some(cluster.clone());
                    self.replace_root(View::Services { cluster });
                }
            }
            Some("tasks") => {
                if let Some(cluster) = need_cluster(self) {
                    self.cluster = Some(cluster.clone());
                    self.replace_root(View::Tasks { cluster, service: None });
                }
            }
            Some("events") => match (self.cluster.clone(), self.service.clone()) {
                (Some(cluster), Some(service)) => self.replace_root(View::Events { cluster, service }),
                _ => self.flash("open a service first, or press e on one", true),
            },
            Some("failures") => {
                if let Some(cluster) = need_cluster(self) {
                    self.cluster = Some(cluster.clone());
                    self.replace_root(View::Failures { cluster, service: None });
                }
            }
            Some("rollout") => match (self.cluster.clone(), self.service.clone()) {
                (Some(cluster), Some(service)) => self.replace_root(View::Rollout { cluster, service }),
                _ => self.flash("open a service first, or press R on one", true),
            },
            Some("taskdefs") => self.replace_root(View::TaskDefFamilies),
            Some("logs") => match arg {
                Some(g) => self.replace_root(View::Logs { src: LogSrc::Group(g), pattern: words[2..].join(" "), opts: LogOpts::default() }),
                None => self.replace_root(View::LogGroups),
            },
            Some("lb") => self.replace_root(View::LoadBalancers),
            Some("tg") => self.replace_root(View::TargetGroups { lb: None }),
            Some("scaling") => self.replace_root(View::ScalableTargets { ns: arg.unwrap_or_else(|| "ecs".into()) }),
            Some("policies") => self.replace_root(View::ScalingPolicies { ns: arg.unwrap_or_else(|| "ecs".into()), resource: None }),
            Some("activities") => self.replace_root(View::ScalingActivities { ns: arg.unwrap_or_else(|| "ecs".into()), resource: None }),
            Some("ec2") => self.replace_root(View::Ec2),
            Some("ecr") => self.replace_root(View::EcrRepos),
            Some("sqs") => self.replace_root(View::Sqs),
            Some("rds") => self.replace_root(View::Rds),
            Some("endpoints") => self.replace_root(View::Endpoints { of: arg }),
            Some("ssm") => self.replace_root(View::Bastions),
            Some("portforwards") => self.replace_root(View::Forwards),
            Some("lambda") => self.replace_root(View::Lambda),
            Some("pulse") => self.replace_root(View::Pulse),
            Some("alarms") => self.replace_root(View::Alarms),
            Some("trail") => self.replace_root(View::Trail { resource: arg }),
            Some("access") => self.replace_root(View::Access),
            Some("secrets") => match arg {
                Some(name) => self.replace_root(View::SecretValue { name, reveal: false }),
                None => self.replace_root(View::Secrets),
            },
            Some("profile") => match arg {
                Some(p) => {
                    self.set_profile(p);
                    self.replace_root(View::Regions)
                }
                None => self.replace_root(View::Profiles),
            },
            Some("region") => match arg {
                Some(r) => {
                    self.set_region(&r);
                    self.replace_root(View::Clusters)
                }
                None => self.replace_root(View::Regions),
            },
            Some("quit") => self.quit = true,
            _ => self.flash(format!("unknown command: {cmd}"), true),
        }
    }
}

/// pbcopy, wl-copy or xclip, whichever exists.
/// A mark row's json; drawn as a rule across the log view.
pub const MARK: &str = "mark";

/// The visible log lines (after the filter) as text: full date, task tag, message.
fn log_text(f: &Frame) -> (String, usize) {
    let vis = f.visible();
    let mut out = String::new();
    for &i in &vis {
        let row = &f.table.rows[i];
        let at = crate::logic::parse_ts(&row.json["timestamp"])
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S%.3f").to_string())
            .unwrap_or_else(|| row.cells[0].clone());
        let task = if row.cells[1].is_empty() { String::new() } else { format!("{} ", row.cells[1]) };
        out.push_str(&format!("{at} {task}{}\n", row.cells[2]));
    }
    (out, vis.len())
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    let tools: [(&str, &[&str]); 3] = [("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
    for (bin, args) in tools {
        let Ok(mut child) = std::process::Command::new(bin).args(args).stdin(std::process::Stdio::piped()).spawn() else { continue };
        child.stdin.take().map(|mut s| s.write_all(text.as_bytes())).transpose().map_err(|e| e.to_string())?;
        return child.wait().map_err(|e| e.to_string()).and_then(|st| if st.success() { Ok(()) } else { Err(format!("{bin} failed")) });
    }
    Err("no pbcopy, wl-copy or xclip".into())
}

/// Region for calls before one is chosen: the profile's configured region, then the environment, then us-east-1.
fn default_region(profile: &str) -> String {
    crate::aws::profiles()
        .into_iter()
        .find(|p| p.0 == profile || (profile.is_empty() && p.0 == "default"))
        .and_then(|p| p.1)
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .unwrap_or_else(|| "us-east-1".into())
}

/// The screens a view sits under when the command line opens it directly (the region list and
/// profile picker above them are added for every jump).
fn parents_of(view: &View) -> Vec<View> {
    match view {
        View::Services { .. } => vec![View::Clusters],
        View::Tasks { cluster, service: None } | View::Failures { cluster, service: None } => {
            vec![View::Clusters, View::Services { cluster: cluster.clone() }]
        }
        View::Logs { src: LogSrc::Group(_), .. } => vec![View::LogGroups],
        View::SecretValue { .. } => vec![View::Secrets],
        View::ScalingPolicies { ns, .. } | View::ScalingActivities { ns, .. } => vec![View::ScalableTargets { ns: ns.clone() }],
        _ => vec![],
    }
}

/// Aliases starting with the typed word, canonical names first.
pub fn completions(input: &str) -> Vec<&'static str> {
    let w = input.trim_start();
    if w.contains(' ') {
        return vec![];
    }
    let mut out: Vec<&str> = COMMANDS.iter().flat_map(|(a, _)| a.iter().copied()).filter(|a| a.starts_with(w)).collect();
    out.sort_by_key(|a| (a.len() != w.len(), !COMMANDS.iter().any(|(x, _)| x[0] == *a)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_filter_and_describe_search() {
        let (re, inv) = filter_re("-f mtrn").unwrap();
        assert!(!inv && re.is_match("my-task-runner") && !re.is_match("runner-task-my"));
        assert!(filter_re("!-f api").unwrap().1);

        let mut f = Frame::new(View::Detail { title: "t".into(), lines: vec![], live: None });
        f.table = Table::text(["a: 1", "b: x", "c: 2", "d: x"].map(String::from).to_vec());
        f.filter = "x".into();
        assert_eq!(f.visible().len(), 4, "describe keeps every line");
        assert!(f.jump_match(0, true));
        assert_eq!(f.sel, 1);
        assert!(f.jump_match(2, true));
        assert_eq!(f.sel, 3);
        assert!(f.jump_match(0, false), "wraps backwards");
        assert_eq!(f.sel, 3);
        f.filter = "zzz".into();
        assert!(!f.jump_match(0, true));
    }

    #[test]
    fn command_line_jumps_stack_their_parents() {
        let titles = |v: &View| parents_of(v).iter().map(View::title).collect::<Vec<_>>();
        assert_eq!(titles(&View::Tasks { cluster: "c".into(), service: None }), ["clusters", "services(c)"]);
        assert_eq!(titles(&View::Logs { src: LogSrc::Group("/g".into()), pattern: String::new(), opts: LogOpts::default() }), ["loggroups"]);
        assert_eq!(titles(&View::ScalingPolicies { ns: "ecs".into(), resource: None }), ["scaling(ecs)"]);
        assert!(titles(&View::LoadBalancers).is_empty(), "region-wide views sit right under the region list");
    }

    #[test]
    fn faults_keep_only_unhealthy_rows() {
        let mut f = Frame::new(View::Clusters);
        for (id, h) in [("a", Health::Ok), ("b", Health::Bad), ("c", Health::Dim), ("d", Health::Warn)] {
            f.table.rows.push(Row { id: id.into(), cells: vec![id.into()], health: h, json: Value::Null });
        }
        f.faults = true;
        assert_eq!(f.visible().iter().map(|&i| f.table.rows[i].id.as_str()).collect::<Vec<_>>(), ["b", "d"]);
        f.filter = "d".into();
        assert_eq!(f.visible().len(), 1, "faults and the filter combine");
    }
}
