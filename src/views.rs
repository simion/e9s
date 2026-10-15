//! Every resource view: what it fetches, its columns, its health colour, where Enter goes.

use crate::aws::{truncated, Aws, AwsError};
use crate::logic::*;
use chrono::{Local, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;

macro_rules! args {
    ($($x:expr),* $(,)?) => { vec![$($x.to_string()),*] };
}

pub const MAX_DESCRIBE_SERVICES: usize = 10;
pub const MAX_DESCRIBE_TASKS: usize = 100;
const LOG_MAX_ITEMS: usize = 1000;
/// A 1m to 1h window reads further than the tail's first screen, per batch of 100 streams.
const LOG_SINCE_MAX_ITEMS: usize = 5000;

#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub cells: Vec<String>,
    pub health: Health,
    pub json: Value,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub health: Health,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Row>,
    pub notes: Vec<Note>,
    /// Set by log views: what `f` live-tails.
    pub tail: Option<LogSel>,
    /// Second pane under the table (rollout: the service event stream), never truncated.
    pub panel: Vec<(Health, String)>,
    /// A column drawn as a full-width line under its row instead of in the grid (like colspan).
    pub detail: Option<usize>,
    /// What an empty result means for this view, shown instead of a bare "no resources".
    pub empty: Option<String>,
}

impl Table {
    /// A trailing REASON or WHAT IT DOES column becomes the detail line under each row.
    fn new(h: &[&str]) -> Self {
        let detail = matches!(h.last(), Some(&"REASON") | Some(&"WHAT IT DOES")).then(|| h.len() - 1);
        Table { headers: h.iter().map(|x| x.to_string()).collect(), detail, ..Default::default() }
    }
    fn row(&mut self, id: impl Into<String>, health: Health, json: Value, cells: Vec<String>) {
        self.rows.push(Row { id: id.into(), cells, health, json });
    }
    fn note(&mut self, health: Health, t: impl Into<String>) {
        self.notes.push(Note { health, text: t.into() });
    }
    fn info(&mut self, t: impl Into<String>) {
        self.note(Health::None, t);
    }
    fn bad(&mut self, t: impl Into<String>) {
        self.note(Health::Bad, t);
    }
    /// A sub-call failed: keep what we have, say loudly what is missing. Expired SSO aborts the view.
    fn soft(&mut self, r: Result<Value, AwsError>, what: &str) -> Result<Option<Value>, AwsError> {
        match r {
            Ok(v) => {
                let fails: Vec<&Value> = arr(&v, "/failures").iter().chain(arr(&v, "/Failures")).collect();
                let missing = fails.iter().filter(|f| s(f, "/reason") == "MISSING").count();
                for f in fails.iter().filter(|f| s(f, "/reason") != "MISSING") {
                    self.bad(format!("{what} failure: {} {}", s(f, "/arn"), s(f, "/reason")));
                }
                if missing > 0 {
                    self.note(Health::Warn, format!("{missing} item(s) were gone by the time they were described (MISSING): ECS forgets stopped tasks after about an hour"));
                }
                Ok(Some(v))
            }
            Err(e @ AwsError::SsoExpired(_)) => Err(e),
            Err(e) => {
                self.bad(format!("{what}: {e} (list INCOMPLETE)"));
                Ok(None)
            }
        }
    }
    fn trunc(&mut self, v: &Value, what: &str) {
        if truncated(v) {
            self.bad(format!("{what}: more results exist, list is TRUNCATED"));
        }
    }
    pub fn text(lines: Vec<String>) -> Self {
        let mut t = Table::new(&["LINE"]);
        for (i, l) in lines.into_iter().enumerate() {
            t.row(i.to_string(), Health::None, Value::Null, vec![l]);
        }
        t
    }
}

/// Run `f` over `items` on up to 8 threads, keeping order.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    par_map_n(items, 8, f)
}

pub fn par_map_n<T: Sync, R: Send>(items: &[T], max: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    let workers = n.clamp(1, max);
    let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
    std::thread::scope(|sc| {
        let f = &f;
        let hs: Vec<_> = (0..workers)
            .map(|w| sc.spawn(move || (w..n).step_by(workers).map(|i| (i, f(&items[i]))).collect::<Vec<_>>()))
            .collect();
        for h in hs {
            for (i, r) in h.join().expect("worker panicked") {
                out[i] = Some(r);
            }
        }
    });
    out.into_iter().map(|r| r.expect("every index filled")).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum LogSrc {
    Group(String),
    Stream { group: String, stream: String },
    Service { cluster: String, service: String },
    Task { cluster: String, task: String, container: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct LogSel {
    pub group: String,
    pub streams: Vec<String>,
    pub prefix: Option<String>,
    pub pattern: String,
    /// Streams span several containers, so each line's tag names the container too.
    pub containers: bool,
}

/// Where a log view starts, as k9s's 0-6 keys: the newest lines, the first ones, or a window back from now.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Since {
    #[default]
    Tail,
    Head,
    Secs(i64),
}

impl Since {
    pub fn label(self) -> String {
        match self {
            Since::Tail => "tail".into(),
            Since::Head => "head".into(),
            Since::Secs(s) => fmt_dur(s),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct LogOpts {
    pub since: Since,
    /// Service logs: every container of each task, not just the main one (k9s `a`).
    pub all: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MetricTarget {
    Service { cluster: String, service: String },
    Lb { arn: String },
    Tg { arn: String, lb: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum View {
    Profiles,
    /// k9s ctrl-a: every command and its aliases.
    Aliases,
    Regions,
    Clusters,
    Services { cluster: String },
    Tasks { cluster: String, service: Option<String> },
    Containers { cluster: String, task: String },
    Events { cluster: String, service: String },
    Rollout { cluster: String, service: String },
    /// Failed task stops and problem events of the last hour, for a cluster or one service.
    Failures { cluster: String, service: Option<String> },
    TaskDefFamilies,
    TaskDefRevisions { family: String },
    TaskDefContainers { arn: String },
    LogGroups,
    LogStreams { group: String },
    Logs { src: LogSrc, pattern: String, opts: LogOpts },
    LoadBalancers,
    TargetGroups { lb: Option<(String, String)> },
    Targets { arn: String, name: String, lb: Option<String> },
    ScalableTargets { ns: String },
    ScalingPolicies { ns: String, resource: Option<String> },
    ScalingActivities { ns: String, resource: Option<String> },
    Metrics { target: MetricTarget },
    Ec2,
    EcrRepos,
    EcrImages { repo: String },
    Sqs,
    Rds,
    Lambda,
    Detail { title: String, lines: Vec<String> },
}

pub const REGIONS: &[&str] = &[
    "us-east-1", "us-east-2", "us-west-1", "us-west-2", "ca-central-1", "eu-west-1", "eu-west-2", "eu-central-1",
    "eu-north-1", "ap-south-1", "ap-southeast-1", "ap-southeast-2", "ap-northeast-1", "ap-northeast-2", "sa-east-1",
];

fn short_task(task: &str) -> &str {
    let id = short(task);
    &id[..id.len().min(8)]
}

impl View {
    pub fn title(&self) -> String {
        match self {
            View::Profiles => "profiles".into(),
            View::Aliases => "aliases".into(),
            View::Regions => "regions".into(),
            View::Clusters => "clusters".into(),
            View::Services { cluster } => format!("services({cluster})"),
            View::Tasks { cluster, service: None } => format!("tasks({cluster})"),
            View::Tasks { service: Some(s), .. } => format!("tasks({s})"),
            View::Containers { task, .. } => format!("containers({})", short_task(task)),
            View::Events { service, .. } => format!("events({service})"),
            View::Rollout { service, .. } => format!("rollout({service})"),
            View::Failures { cluster, service } => format!("failures({})", service.as_ref().unwrap_or(cluster)),
            View::TaskDefFamilies => "taskdefs".into(),
            View::TaskDefRevisions { family } => format!("revisions({family})"),
            View::TaskDefContainers { arn } => format!("taskdef({})", taskdef(arn)),
            View::LogGroups => "loggroups".into(),
            View::LogStreams { group } => format!("streams({group})"),
            View::Logs { src, pattern, .. } => {
                let what = match src {
                    LogSrc::Group(g) => g.clone(),
                    LogSrc::Stream { stream, .. } => stream.clone(),
                    LogSrc::Service { service, .. } => service.clone(),
                    LogSrc::Task { task, container, .. } => {
                        format!("{}{}", short_task(task), container.as_ref().map(|c| format!("/{c}")).unwrap_or_default())
                    }
                };
                if pattern.is_empty() { format!("logs({what})") } else { format!("logs({what} ~ {pattern})") }
            }
            View::LoadBalancers => "loadbalancers".into(),
            View::TargetGroups { lb: None } => "targetgroups".into(),
            View::TargetGroups { lb: Some((_, n)) } => format!("targetgroups({n})"),
            View::Targets { name, .. } => format!("targets({name})"),
            View::ScalableTargets { ns } => format!("scaling({ns})"),
            View::ScalingPolicies { resource, ns } => format!("policies({})", resource.as_deref().unwrap_or(ns)),
            View::ScalingActivities { resource, ns } => format!("activities({})", resource.as_deref().unwrap_or(ns)),
            View::Metrics { target } => match target {
                MetricTarget::Service { service, .. } => format!("metrics({service})"),
                MetricTarget::Lb { arn } | MetricTarget::Tg { arn, .. } => format!("metrics({})", lb_name(arn)),
            },
            View::Ec2 => "ec2".into(),
            View::EcrRepos => "ecr".into(),
            View::EcrImages { repo } => format!("images({repo})"),
            View::Sqs => "sqs".into(),
            View::Rds => "rds".into(),
            View::Lambda => "lambda".into(),
            View::Detail { title, .. } => title.clone(),
        }
    }

    /// Text views render as scrolling lines (wrap, follow), the rest as tables.
    pub fn is_text(&self) -> bool {
        matches!(self, View::Logs { .. } | View::Detail { .. })
    }

    pub fn auto_refresh(&self) -> bool {
        !matches!(self, View::Logs { .. } | View::Detail { .. } | View::Profiles | View::Regions | View::Aliases)
    }

    /// Where Enter goes.
    pub fn drill(&self, r: &Row) -> Option<View> {
        let j = &r.json;
        Some(match self {
            View::Clusters => View::Services { cluster: r.id.clone() },
            View::Services { cluster } => View::Tasks { cluster: cluster.clone(), service: Some(r.id.clone()) },
            View::Tasks { cluster, .. } | View::Rollout { cluster, .. } => View::Containers { cluster: cluster.clone(), task: r.id.clone() },
            View::Failures { cluster, .. } if j.get("taskArn").is_some() => View::Containers { cluster: cluster.clone(), task: r.id.clone() },
            View::Failures { cluster, .. } => View::Events { cluster: cluster.clone(), service: r.cells[1].clone() },
            View::TaskDefFamilies => View::TaskDefRevisions { family: r.id.clone() },
            View::TaskDefRevisions { .. } => View::TaskDefContainers { arn: r.id.clone() },
            View::LogGroups => View::LogStreams { group: r.id.clone() },
            View::LogStreams { group } => View::Logs { src: LogSrc::Stream { group: group.clone(), stream: r.id.clone() }, pattern: String::new(), opts: LogOpts::default() },
            View::LoadBalancers => View::TargetGroups { lb: Some((r.id.clone(), s(j, "/LoadBalancerName"))) },
            View::TargetGroups { .. } => View::Targets {
                arn: r.id.clone(),
                name: s(j, "/TargetGroupName"),
                lb: strs(j, "/LoadBalancerArns").into_iter().next(),
            },
            View::ScalableTargets { ns } => View::ScalingActivities { ns: ns.clone(), resource: Some(r.id.clone()) },
            View::EcrRepos => View::EcrImages { repo: r.id.clone() },
            _ => return None,
        })
    }

    /// Where `l` goes.
    pub fn log_src(&self, r: &Row) -> Option<LogSrc> {
        Some(match self {
            View::Services { cluster } => LogSrc::Service { cluster: cluster.clone(), service: r.id.clone() },
            View::Events { cluster, service } => LogSrc::Service { cluster: cluster.clone(), service: service.clone() },
            View::Tasks { cluster, .. } | View::Rollout { cluster, .. } => LogSrc::Task { cluster: cluster.clone(), task: r.id.clone(), container: None },
            View::Failures { cluster, .. } if r.json.get("taskArn").is_some() => LogSrc::Task { cluster: cluster.clone(), task: r.id.clone(), container: None },
            View::Failures { cluster, .. } => LogSrc::Service { cluster: cluster.clone(), service: r.cells[1].clone() },
            View::Containers { cluster, task } => LogSrc::Task { cluster: cluster.clone(), task: task.clone(), container: Some(r.id.clone()) },
            View::LogGroups => LogSrc::Group(r.id.clone()),
            View::LogStreams { group } => LogSrc::Stream { group: group.clone(), stream: r.id.clone() },
            View::Lambda => {
                let g = s(&r.json, "/LoggingConfig/LogGroup");
                LogSrc::Group(if g.is_empty() { format!("/aws/lambda/{}", r.id) } else { g })
            }
            _ => return None,
        })
    }

    /// Where `m` goes.
    pub fn metrics(&self, r: Option<&Row>) -> Option<MetricTarget> {
        match (self, r) {
            (View::Services { cluster }, Some(r)) => Some(MetricTarget::Service { cluster: cluster.clone(), service: r.id.clone() }),
            (View::Rollout { cluster, service } | View::Events { cluster, service }, _) => {
                Some(MetricTarget::Service { cluster: cluster.clone(), service: service.clone() })
            }
            (View::LoadBalancers, Some(r)) => Some(MetricTarget::Lb { arn: r.id.clone() }),
            (View::TargetGroups { .. }, Some(r)) => {
                Some(MetricTarget::Tg { arn: r.id.clone(), lb: strs(&r.json, "/LoadBalancerArns").into_iter().next() })
            }
            (View::Targets { arn, lb, .. }, _) => Some(MetricTarget::Tg { arn: arn.clone(), lb: lb.clone() }),
            _ => None,
        }
    }

    pub fn fetch(&self, aws: &Aws) -> Result<Table, AwsError> {
        match self {
            View::Aliases => {
                let mut t = Table::new(&["COMMAND", "ALIASES", "VIEW"]);
                t.info("Enter opens the view (k9s ctrl-a) · / filters");
                for (names, what) in crate::app::COMMANDS {
                    t.row(names[0], Health::None, Value::Null, vec![names[0].to_string(), names[1..].join(" "), what.to_string()]);
                }
                Ok(t)
            }
            View::Profiles => {
                let mut t = Table::new(&["PROFILE", "ACCOUNT", "ROLE", "REGION", "SSO SESSION"]);
                t.info("a profile names an account and a role to sign in as (Kubernetes: a kubeconfig context) · Enter picks one");
                for (p, kv) in crate::aws::profile_details() {
                    let g = |k: &str| kv.get(k).cloned().unwrap_or_default();
                    let role = if kv.contains_key("role_arn") { short(&g("role_arn")).to_string() } else { g("sso_role_name") };
                    let account = if kv.contains_key("sso_account_id") {
                        g("sso_account_id")
                    } else {
                        g("role_arn").split(':').nth(4).unwrap_or("").to_string()
                    };
                    t.row(p.clone(), Health::None, json!({"profile": p, "region": kv.get("region")}), vec![p, account, role, g("region"), g("sso_session")]);
                }
                t.empty = Some("no profiles in ~/.aws/config or ~/.aws/credentials".into());
                Ok(t)
            }
            View::Regions => regions(aws),
            View::Detail { lines, .. } => Ok(Table::text(lines.clone())),
            View::Clusters => clusters(aws),
            View::Services { cluster } => services(aws, cluster),
            View::Tasks { cluster, service } => tasks(aws, cluster, service.as_deref()),
            View::Containers { cluster, task } => containers(aws, cluster, task),
            View::Events { cluster, service } => events(aws, cluster, service),
            View::Rollout { cluster, service } => rollout(aws, cluster, service),
            View::Failures { cluster, service } => failures(aws, cluster, service.as_deref()),
            View::TaskDefFamilies => {
                let v = aws.call("ecs", "list-task-definition-families", &args!["--status", "ACTIVE"])?;
                let mut t = Table::new(&["FAMILY"]);
                for f in strs(&v, "/families") {
                    t.row(f.clone(), Health::None, json!({ "family": f }), vec![f]);
                }
                Ok(t)
            }
            View::TaskDefRevisions { family } => taskdef_revisions(aws, family),
            View::TaskDefContainers { arn } => taskdef_containers(aws, arn),
            View::LogGroups => log_groups(aws),
            View::LogStreams { group } => {
                let v = aws.call("logs", "describe-log-streams", &args!["--log-group-name", group, "--order-by", "LastEventTime", "--descending", "--max-items", 200])?;
                let mut t = Table::new(&["STREAM", "LAST EVENT", "CREATED"]);
                if truncated(&v) {
                    t.bad("showing the 200 most recently active streams only; older streams exist");
                }
                for st in arr(&v, "/logStreams") {
                    let name = s(st, "/logStreamName");
                    t.row(name.clone(), Health::None, st.clone(), vec![name, age_at(st, "/lastEventTimestamp"), age_at(st, "/creationTime")]);
                }
                Ok(t)
            }
            View::Logs { src, pattern, opts } => logs(aws, src, pattern, *opts),
            View::LoadBalancers => load_balancers(aws),
            View::TargetGroups { lb } => target_groups(aws, lb.as_ref().map(|x| x.0.as_str())),
            View::Targets { arn, .. } => targets(aws, arn),
            View::ScalableTargets { ns } => scalable_targets(aws, ns),
            View::ScalingPolicies { ns, resource } => scaling_policies(aws, ns, resource.as_deref()),
            View::ScalingActivities { ns, resource } => scaling_activities(aws, ns, resource.as_deref()),
            View::Metrics { target } => metrics(aws, target),
            View::Ec2 => ec2(aws),
            View::EcrRepos => ecr_repos(aws),
            View::EcrImages { repo } => ecr_images(aws, repo),
            View::Sqs => sqs(aws),
            View::Rds => rds(aws),
            View::Lambda => lambda(aws),
        }
    }
}

/// Every region enabled for the account, scanned in parallel for ECS clusters, services and running tasks.
fn regions(aws: &Aws) -> Result<Table, AwsError> {
    let mut t = Table::new(&["REGION", "ECS CLUSTERS", "SERVICES", "RUNNING TASKS", "CLUSTER NAMES"]);
    let names: Vec<String> = match aws.call("ec2", "describe-regions", &[]) {
        Ok(v) => arr(&v, "/Regions").iter().map(|r| s(r, "/RegionName")).collect(),
        Err(e @ AwsError::SsoExpired(_)) => return Err(e),
        Err(e) => {
            t.bad(format!("describe-regions failed, scanning a fixed list instead: {e}"));
            REGIONS.iter().map(|r| r.to_string()).collect()
        }
    };
    let scans = par_map_n(&names, 16, |r| -> Result<(usize, i64, i64, Vec<String>), AwsError> {
        let a = aws.in_region(r);
        let arns = strs(&a.call("ecs", "list-clusters", &[])?, "/clusterArns");
        let (mut svcs, mut tasks, mut cl) = (0, 0, vec![]);
        for chunk in arns.chunks(100) {
            let mut args = args!["--clusters"];
            args.extend(chunk.iter().cloned());
            for c in arr(&a.call("ecs", "describe-clusters", &args)?, "/clusters") {
                svcs += n(c, "/activeServicesCount");
                tasks += n(c, "/runningTasksCount");
                cl.push(s(c, "/clusterName"));
            }
        }
        Ok((arns.len(), svcs, tasks, cl))
    });
    let mut rows = vec![];
    for (r, scan) in names.iter().zip(scans) {
        match scan {
            Ok(x) => rows.push((r.clone(), x)),
            Err(e @ AwsError::SsoExpired(_)) => return Err(e),
            Err(e) => t.bad(format!("{r}: {e}")),
        }
    }
    rows.sort_by_key(|(r, (c, _, tasks, _))| (*c == 0, std::cmp::Reverse(*tasks), r.clone()));
    let with = rows.iter().filter(|(_, x)| x.0 > 0).count();
    t.info(format!("{with} of {} enabled regions have ECS clusters · Enter opens a region", names.len()));
    for (r, (c, svcs, tasks, cl)) in rows {
        let h = if c > 0 { Health::Ok } else { Health::Dim };
        t.row(r.clone(), h, json!({ "region": r, "clusters": cl }), vec![r, c.to_string(), svcs.to_string(), tasks.to_string(), cl.join(", ")]);
    }
    Ok(t)
}

fn lb_name(arn: &str) -> String {
    // .../loadbalancer/app/<name>/<id> or .../targetgroup/<name>/<id>
    let parts: Vec<&str> = arn.split('/').collect();
    let i = if arn.contains(":loadbalancer/") { 2 } else { 1 };
    parts.get(i).unwrap_or(&arn).to_string()
}

// ---------- ECS ----------

fn clusters(aws: &Aws) -> Result<Table, AwsError> {
    let arns = strs(&aws.call("ecs", "list-clusters", &[])?, "/clusterArns");
    let mut t = Table::new(&["NAME", "STATUS", "SERVICES", "RUNNING", "PENDING", "INSTANCES", "CAPACITY", "INSIGHTS"]);
    for chunk in arns.chunks(100) {
        let mut a = args!["--include", "SETTINGS", "--clusters"];
        a.extend(chunk.iter().cloned());
        let Some(v) = t.soft(aws.call("ecs", "describe-clusters", &a), "describe-clusters")? else { continue };
        for c in arr(&v, "/clusters") {
            let name = s(c, "/clusterName");
            t.row(name.clone(), health_of(&s(c, "/status")), c.clone(), vec![
                name,
                s(c, "/status"),
                s(c, "/activeServicesCount"),
                s(c, "/runningTasksCount"),
                s(c, "/pendingTasksCount"),
                s(c, "/registeredContainerInstancesCount"),
                strs(c, "/capacityProviders").join(", "),
                arr(c, "/settings").iter().find(|x| s(x, "/name") == "containerInsights").map(|x| s(x, "/value")).unwrap_or_default(),
            ]);
        }
    }
    Ok(t)
}

/// DescribeServices takes at most 10 names per call. A failed chunk becomes a loud note, never an empty list.
pub fn describe_services(aws: &Aws, cluster: &str, names: &[String], t: &mut Table) -> Result<Vec<Value>, AwsError> {
    let chunks: Vec<&[String]> = names.chunks(MAX_DESCRIBE_SERVICES).collect();
    let results = par_map(&chunks, |c| {
        let mut a = args!["--cluster", cluster, "--services"];
        a.extend(c.iter().cloned());
        aws.call("ecs", "describe-services", &a)
    });
    let mut out = vec![];
    for (c, r) in chunks.iter().zip(results) {
        let what = format!("describe-services ({} of {} services)", c.len(), names.len());
        if let Some(v) = t.soft(r, &what)? {
            out.extend(arr(&v, "/services").iter().cloned());
        }
    }
    Ok(out)
}

/// Service names change rarely; refreshes reuse the list for a minute so a refresh is one call deep.
fn cached_service_arns(aws: &Aws, cluster: &str) -> Result<Vec<String>, AwsError> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Vec<String>)>>> = OnceLock::new();
    let key = format!("{}/{}/{cluster}", aws.ctx.profile, aws.ctx.region);
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, v)) = cache.lock().unwrap().get(&key) {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(v.clone());
        }
    }
    let v = strs(&aws.call("ecs", "list-services", &args!["--cluster", cluster])?, "/serviceArns");
    cache.lock().unwrap().insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

fn services(aws: &Aws, cluster: &str) -> Result<Table, AwsError> {
    let arns = cached_service_arns(aws, cluster)?;
    let mut t = Table::new(&["NAME", "STATUS", "DESIRED", "RUN(ALL)", "PENDING", "NEW", "OLD", "ROLLOUT", "DEPLOY AGE", "FAILED 1H", "TASKDEF"]);
    t.info("Enter tasks · R rollout watch · e events · F failures · l logs · m metrics · runningCount (RUN(ALL)) sums both generations; NEW is new-running/desired");
    let (mut svcs, stopped) = with_stopped(aws, cluster, None, &mut t, |t| describe_services(aws, cluster, &arns, t))?;
    svcs.sort_by_key(|v| s(v, "/serviceName"));
    for v in svcs {
        let name = s(&v, "/serviceName");
        let (tasks, evs) = (failed_stops(&stopped, &name).len(), problem_events(&v).len());
        let failed = match (tasks, evs) {
            (0, 0) => "-".to_string(),
            (k, 0) => format!("{k} tasks"),
            (0, e) => format!("{e} events"),
            (k, e) => format!("{k} tasks, {e} events"),
        };
        let sp = split_deployments(&v);
        let p = sp.primary.clone();
        let old = if sp.old.is_empty() { "-".to_string() } else { format!("{} ({}x)", sp.old_running(), sp.old.len()) };
        let health = if tasks + evs > 0 { Health::Bad } else { sp.health(&s(&v, "/status")) };
        t.row(name.clone(), health, v.clone(), vec![
            name,
            s(&v, "/status"),
            sp.desired.to_string(),
            sp.running_all.to_string(),
            sp.pending_all.to_string(),
            format!("{}/{}", sp.new_running(), sp.desired),
            old,
            p.as_ref().map(|d| d.rollout.clone()).unwrap_or_default(),
            p.as_ref().map(|d| age(&Value::String(d.created.clone()))).unwrap_or_default(),
            failed,
            p.map(|d| d.taskdef).unwrap_or_default(),
        ]);
    }
    Ok(t)
}

/// k9s pod colours: scheduler scale-in of an old task is normal (dim), crashes and failed health checks are red.
pub fn task_health(t: &Value) -> Health {
    let last = s(t, "/lastStatus");
    if last == "STOPPED" || s(t, "/desiredStatus") == "STOPPED" && last != "RUNNING" {
        let reason = s(t, "/stoppedReason").to_lowercase();
        let code = s(t, "/stopCode");
        let drained = reason.starts_with("scaling activity initiated by") || code == "UserInitiated";
        let bad_exit = arr(t, "/containers").iter().any(|c| c.get("exitCode").is_some_and(|e| !e.is_null()) && n(c, "/exitCode") != 0);
        if code == "TaskFailedToStart" || code == "EssentialContainerExited" || reason.contains("health check") || (bad_exit && !drained) {
            Health::Bad
        } else if code == "SpotInterruption" || code == "TerminationNotice" {
            Health::Warn
        } else {
            Health::Dim
        }
    } else if s(t, "/healthStatus") == "UNHEALTHY" {
        Health::Bad
    } else if last == "RUNNING" {
        Health::Ok
    } else {
        Health::Warn
    }
}

/// Why a task stopped, by generation. A scheduler scale-in of the old generation is a normal drain;
/// only health-check failures, failed starts and non-zero exits outside a drain are failures.
pub fn stop_kind(x: &Value, sp: &Split) -> &'static str {
    let reason = s(x, "/stoppedReason").to_lowercase();
    let code = s(x, "/stopCode");
    let drain = reason.starts_with("scaling activity initiated by");
    let bad_exit = arr(x, "/containers").iter().any(|c| c.get("exitCode").is_some_and(|e| !e.is_null()) && n(c, "/exitCode") != 0);
    let started_by = s(x, "/startedBy");
    let old = sp.old.iter().any(|d| d.id == started_by) || sp.primary.as_ref().is_some_and(|p| p.id != started_by);
    match () {
        _ if reason.contains("health check") => "failed health check",
        _ if code == "TaskFailedToStart" => "failed to start",
        _ if code == "EssentialContainerExited" || (bad_exit && !drain) => "crashed (non-zero exit)",
        _ if drain && old => "drained (old gen)",
        _ if drain => "scaled in (new gen)",
        _ if code == "UserInitiated" => "stopped by a user",
        _ => "other",
    }
}

/// For display: `arn:aws:elasticloadbalancing:<region>:<acct>:targetgroup/<name>/<id>` reads as `targetgroup/<name>`.
pub fn short_arns(text: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"arn:aws:elasticloadbalancing:[^:\s]+:\d+:((?:targetgroup|loadbalancer)/[^/\s)]+(?:/[^/\s)]+)?)/[0-9a-f]+").unwrap())
        .replace_all(text, "$1")
        .into_owned()
}

/// Everything around a service's tasks that explains them.
#[derive(Default)]
struct TaskCtx {
    split: Option<Split>,
    events: Vec<Value>,
    /// task private IP -> (target state, reason + description), from the service's target groups.
    targets: HashMap<String, (String, String)>,
}

pub const TASK_HEADERS: &[&str] =
    &["TASK", "GEN", "TASKDEF", "LAST", "DESIRED", "HEALTH", "TARGET", "AZ", "CPU/MEM", "STARTED", "STOPPED", "AGE", "EXIT", "REASON"];

fn task_ip(x: &Value) -> Option<String> {
    arr(x, "/containers")
        .iter()
        .flat_map(|c| arr(c, "/networkInterfaces"))
        .map(|ni| s(ni, "/privateIpv4Address"))
        .find(|ip| !ip.is_empty())
}

/// The ELB "is unhealthy in" service event closest before a task stopped. Matched by time, labelled as such.
fn elb_event_before<'a>(events: &'a [Value], stopped: &Value) -> Option<(String, &'a str)> {
    let stop = parse_ts(stopped)?;
    events.iter().find_map(|e| {
        let at = parse_ts(&e["createdAt"])?;
        let gap = (stop - at).num_seconds();
        let m = e["message"].as_str()?;
        ((0..=600).contains(&gap) && m.contains("is unhealthy in")).then(|| (fmt_dur(gap), m))
    })
}

fn task_row(t: &mut Table, x: &Value, cx: &TaskCtx) {
    let started_by = s(x, "/startedBy");
    let generation = match &cx.split {
        Some(sp) if sp.primary.as_ref().is_some_and(|p| p.id == started_by) => "new",
        Some(sp) if sp.old.iter().any(|d| d.id == started_by) => "old",
        Some(_) => "gone",
        None => "-",
    };
    let target = task_ip(x).and_then(|ip| cx.targets.get(&ip).cloned());
    let mut health = task_health(x);
    let mut reason = vec![];
    let stopped = s(x, "/lastStatus") == "STOPPED";
    if !s(x, "/stoppedReason").is_empty() {
        reason.push(format!("{}: {}", s(x, "/stopCode"), s(x, "/stoppedReason")));
    }
    for c in arr(x, "/containers") {
        let exit = c.get("exitCode").filter(|e| !e.is_null()).map(|e| e.to_string());
        let (r, h) = (s(c, "/reason"), s(c, "/healthStatus"));
        if exit.as_deref().is_some_and(|e| e != "0") || !r.is_empty() || h == "UNHEALTHY" {
            let mut part = format!("[{}]", s(c, "/name"));
            if let Some(e) = exit {
                part += &format!(" exit {e}");
            }
            if !r.is_empty() {
                part += &format!(" {r}");
            }
            if h == "UNHEALTHY" {
                part += " container health check UNHEALTHY";
            }
            reason.push(part);
        }
    }
    if let Some((state, why)) = &target {
        match state.as_str() {
            "unhealthy" => {
                health = Health::Bad;
                reason.push(format!("target unhealthy: {why}"));
            }
            "draining" | "initial" if health == Health::Ok => health = Health::Warn,
            _ => {}
        }
    }
    if stopped && s(x, "/stoppedReason").to_lowercase().contains("health check") {
        if let Some((gap, m)) = elb_event_before(&cx.events, &x["stoppedAt"]) {
            reason.push(format!("service event {gap} before stop: {m}"));
        }
    }
    // Only non-zero exits; all-zero shows as 0. Each container's code is also in REASON and in describe.
    let with_exit: Vec<&Value> = arr(x, "/containers").iter().filter(|c| c.get("exitCode").is_some_and(|e| !e.is_null())).collect();
    let mut exits: Vec<String> = with_exit.iter().filter(|c| n(c, "/exitCode") != 0).map(|c| format!("{}={}", s(c, "/name"), s(c, "/exitCode"))).collect();
    if exits.is_empty() && !with_exit.is_empty() {
        exits.push("0".into());
    }
    let arn = s(x, "/taskArn");
    t.row(arn.clone(), health, x.clone(), vec![
        short(&arn).chars().take(12).collect(),
        generation.into(),
        taskdef(&s(x, "/taskDefinitionArn")).to_string(),
        s(x, "/lastStatus"),
        s(x, "/desiredStatus"),
        s(x, "/healthStatus"),
        target.map(|(st, _)| st).unwrap_or_else(|| "-".into()),
        s(x, "/availabilityZone"),
        format!("{}/{}", s(x, "/cpu"), s(x, "/memory")),
        age_at(x, "/startedAt"),
        age_at(x, "/stoppedAt"),
        age_at(x, "/createdAt"),
        exits.join(" "),
        short_arns(&reason.join(" · ")),
    ]);
}

/// ListTasks for RUNNING and STOPPED merged, DescribeTasks in chunks of 100. A failed list aborts the view.
pub fn list_tasks(aws: &Aws, cluster: &str, service: Option<&str>, t: &mut Table) -> Result<Vec<Value>, AwsError> {
    list_tasks_in(aws, cluster, service, &["RUNNING", "STOPPED"], t)
}

fn list_tasks_in(aws: &Aws, cluster: &str, service: Option<&str>, statuses: &[&str], t: &mut Table) -> Result<Vec<Value>, AwsError> {
    let lists = par_map(statuses, |st| {
        let mut a = args!["--cluster", cluster, "--desired-status", st];
        if let Some(sv) = service {
            a.extend(args!["--service-name", sv]);
        }
        aws.call("ecs", "list-tasks", &a)
    });
    let mut arns = vec![];
    for l in lists {
        arns.extend(strs(&l?, "/taskArns"));
    }
    let chunks: Vec<&[String]> = arns.chunks(MAX_DESCRIBE_TASKS).collect();
    let results = par_map(&chunks, |c| {
        let mut a = args!["--cluster", cluster, "--tasks"];
        a.extend(c.iter().cloned());
        aws.call("ecs", "describe-tasks", &a)
    });
    let mut all = vec![];
    for (c, r) in chunks.iter().zip(results) {
        if let Some(mut v) = t.soft(r, &format!("describe-tasks ({} of {} tasks)", c.len(), arns.len()))? {
            redact(&mut v);
            all.extend(arr(&v, "/tasks").iter().cloned());
        }
    }
    all.sort_by_key(|x| (s(x, "/lastStatus") == "STOPPED", std::cmp::Reverse(parse_ts(&x["createdAt"]))));
    Ok(all)
}

/// Service, its tasks and its target health, fetched concurrently.
fn service_bundle(aws: &Aws, cluster: &str, service: &str, t: &mut Table) -> Result<(Value, Vec<Value>, TaskCtx, Vec<(String, HashMap<String, usize>)>), AwsError> {
    // Two concurrent chains, each two calls deep: service -> its target health, task lists -> task details.
    let (svc_side, tasks) = std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            let mut tt = Table::default();
            let svc = describe_services(aws, cluster, &[service.to_string()], &mut tt);
            let tg_arns: Vec<String> = match &svc {
                Ok(v) => v.first().map(|x| arr(x, "/loadBalancers").iter().map(|l| s(l, "/targetGroupArn")).filter(|a| !a.is_empty()).collect()).unwrap_or_default(),
                Err(_) => vec![],
            };
            let healths = par_map(&tg_arns, |a| aws.call("elbv2", "describe-target-health", &args!["--target-group-arn", a]));
            (svc, tt.notes, tg_arns, healths)
        });
        let tasks = list_tasks(aws, cluster, Some(service), t);
        (h.join().expect("describe-services thread"), tasks)
    });
    let (svc, notes, tg_arns, healths) = svc_side;
    t.notes.extend(notes);
    let svc = svc?.into_iter().next().ok_or_else(|| AwsError::Api(format!("service {service} not found in {cluster}")))?;
    let tasks = tasks?;
    let mut cx = TaskCtx { split: Some(split_deployments(&svc)), events: arr(&svc, "/events").to_vec(), targets: HashMap::new() };
    let mut counts = vec![];
    for (arn, h) in tg_arns.iter().zip(healths) {
        let name = lb_name(arn);
        let Some(h) = t.soft(h, &format!("describe-target-health {name}"))? else { continue };
        let mut c: HashMap<String, usize> = HashMap::new();
        for d in arr(&h, "/TargetHealthDescriptions") {
            let st = s(d, "/TargetHealth/State");
            *c.entry(st.clone()).or_default() += 1;
            let why = [s(d, "/TargetHealth/Reason"), s(d, "/TargetHealth/Description")].join(" ").trim().to_string();
            cx.targets.insert(s(d, "/Target/Id"), (st, why));
        }
        counts.push((name, c));
    }
    Ok((svc, tasks, cx, counts))
}

fn retention_notes(t: &mut Table, tasks: &[&Value]) {
    let stopped: Vec<&&Value> = tasks.iter().filter(|x| s(x, "/lastStatus") == "STOPPED").collect();
    let failed: Vec<&&Value> = stopped.iter().copied().filter(|x| task_health(x) == Health::Bad).collect();
    if let Some(newest) = failed.first() {
        t.bad(format!(
            "{} task(s) stopped with failures in the last ~hour. Newest ({} ago): {}: {}",
            failed.len(), age_at(newest, "/stoppedAt"), s(newest, "/stopCode"), s(newest, "/stoppedReason"),
        ));
    }
    let msg = if stopped.is_empty() {
        "no STOPPED tasks listed: ECS keeps stopped tasks for about 1 hour, so older failures only show in service events (e)".to_string()
    } else {
        format!("{} STOPPED task(s) shown; ECS keeps stopped tasks for about 1 hour only", stopped.len())
    };
    t.note(Health::Warn, msg);
}

fn tasks(aws: &Aws, cluster: &str, service: Option<&str>) -> Result<Table, AwsError> {
    let mut t = Table::new(TASK_HEADERS);
    let (tasks, cx) = match service {
        Some(sv) => {
            let (svc, tasks, cx, _) = service_bundle(aws, cluster, sv, &mut t)?;
            if let Some(e) = arr(&svc, "/events").first() {
                t.info(format!("newest event ({} ago): {}", age_at(e, "/createdAt"), short_event(&s(e, "/message"))));
            }
            (tasks, cx)
        }
        None => (list_tasks(aws, cluster, None, &mut t)?, TaskCtx::default()),
    };
    retention_notes(&mut t, &tasks.iter().collect::<Vec<_>>());
    for x in &tasks {
        task_row(&mut t, x, &cx);
    }
    // Across a whole cluster the generation is unknown; which service a task belongs to is what matters.
    if service.is_none() {
        t.headers[1] = "SERVICE".into();
        for (r, x) in t.rows.iter_mut().zip(&tasks) {
            r.cells[1] = s(x, "/group").trim_start_matches("service:").to_string();
        }
    }
    t.empty = Some("no tasks running, and none stopped in the last hour".into());
    Ok(t)
}

/// Runs `f` while the cluster's (or service's) STOPPED tasks are listed. ECS keeps them about an hour.
fn with_stopped<R>(aws: &Aws, cluster: &str, service: Option<&str>, t: &mut Table, f: impl FnOnce(&mut Table) -> Result<R, AwsError> + Send) -> Result<(R, Vec<Value>), AwsError>
where
    R: Send,
{
    let mut side = Table::default();
    let (r, stopped) = std::thread::scope(|sc| {
        let h = sc.spawn(|| list_tasks_in(aws, cluster, service, &["STOPPED"], &mut side));
        (f(t), h.join().expect("list-tasks thread"))
    });
    t.notes.extend(side.notes);
    Ok((r?, stopped?))
}

/// A service's stopped tasks that failed (failed health check, failed start, crash), not drains.
fn failed_stops<'a>(stopped: &'a [Value], service: &str) -> Vec<&'a Value> {
    let group = format!("service:{service}");
    stopped.iter().filter(|x| s(x, "/group") == group && task_health(x) == Health::Bad).collect()
}

/// A service's problem events (event_health red) of the last hour.
fn problem_events(svc: &Value) -> Vec<&Value> {
    let since = Utc::now() - chrono::Duration::hours(1);
    arr(svc, "/events")
        .iter()
        .filter(|e| parse_ts(&e["createdAt"]).is_some_and(|at| at >= since) && event_health(&s(e, "/message")) == Health::Bad)
        .collect()
}

/// Everything that went wrong in the last hour, newest first: failed task stops and problem service events.
fn failures(aws: &Aws, cluster: &str, service: Option<&str>) -> Result<Table, AwsError> {
    let mut t = Table::new(&["AGE", "SERVICE", "WHAT", "TASK", "REASON"]);
    let names = match service {
        Some(sv) => vec![sv.to_string()],
        None => cached_service_arns(aws, cluster)?,
    };
    let (svcs, stopped) = with_stopped(aws, cluster, service, &mut t, |t| describe_services(aws, cluster, &names, t))?;
    let mut rows: Vec<(Option<chrono::DateTime<Utc>>, Row)> = vec![];
    for svc in &svcs {
        let name = s(svc, "/serviceName");
        let cx = TaskCtx { split: Some(split_deployments(svc)), events: arr(svc, "/events").to_vec(), targets: HashMap::new() };
        for x in failed_stops(&stopped, &name) {
            // The REASON task_row builds for the task list, so both views explain a stop the same way.
            let mut scratch = Table::new(TASK_HEADERS);
            task_row(&mut scratch, x, &cx);
            let reason = scratch.rows.pop().and_then(|r| r.cells.last().cloned()).unwrap_or_default();
            let kind = stop_kind(x, cx.split.as_ref().unwrap());
            let arn = s(x, "/taskArn");
            rows.push((parse_ts(&x["stoppedAt"]), Row {
                id: arn.clone(), health: Health::Bad, json: x.clone(),
                cells: vec![age_at(x, "/stoppedAt"), name.clone(), kind.into(), short(&arn).chars().take(12).collect(), reason],
            }));
        }
        for e in problem_events(svc) {
            rows.push((parse_ts(&e["createdAt"]), Row {
                id: s(e, "/id"), health: Health::Bad, json: e.clone(),
                cells: vec![age_at(e, "/createdAt"), name.clone(), "service event".into(), String::new(), short_event(&s(e, "/message"))],
            }));
        }
    }
    rows.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    t.rows = rows.into_iter().map(|(_, r)| r).collect();
    t.info("last hour: ECS keeps stopped tasks about an hour and each service's last 100 events · Enter opens a task's containers or a service's events · l logs · d describe");
    t.empty = Some("nothing failed in the last hour: no failed task stops and no problem service events".into());
    Ok(t)
}

/// Problems red, a settled service green, the ordinary steps of a rollout plain (Kubernetes "Normal" events).
pub fn event_health(m: &str) -> Health {
    let l = m.to_lowercase();
    if ["unhealthy", "failed", "unable", "error"].iter().any(|k| l.contains(k)) {
        Health::Bad
    } else if l.contains("steady state") {
        Health::Ok
    } else {
        Health::None
    }
}

/// For display: ARNs to their names and `(task <32 hex>)` to the 12-character id the task list shows.
/// Matching and counting always use the raw message.
pub fn short_event(m: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\(task ([0-9a-f]{12})[0-9a-f]{20}\)").unwrap());
    re.replace_all(&short_arns(m), "$1").into_owned()
}

fn events(aws: &Aws, cluster: &str, service: &str) -> Result<Table, AwsError> {
    let mut t = Table::new(&["AGE", "MESSAGE"]);
    let svcs = describe_services(aws, cluster, &[service.to_string()], &mut t)?;
    let svc = svcs.first().ok_or_else(|| AwsError::Api(format!("service {service} not found in {cluster}")))?;
    for e in arr(svc, "/events") {
        let m = s(e, "/message");
        t.row(s(e, "/id"), event_health(&m), e.clone(), vec![age_at(e, "/createdAt"), short_event(&m)]);
    }
    Ok(t)
}

/// The rollout watch: both generations, PRIMARY tasks plus everything stopped since it began, target health, churn, events.
fn rollout(aws: &Aws, cluster: &str, service: &str) -> Result<Table, AwsError> {
    let mut t = Table::new(TASK_HEADERS);
    let (svc, tasks, cx, tgs) = service_bundle(aws, cluster, service, &mut t)?;
    let sp = cx.split.clone().unwrap_or_else(|| split_deployments(&svc));
    let Some(p) = sp.primary.clone() else {
        t.bad("service has no PRIMARY deployment");
        return Ok(t);
    };
    let start = parse_ts(&Value::String(p.created.clone()));
    let elapsed = start.map(|s| (Utc::now() - s).num_seconds()).unwrap_or(0);
    let pdep = arr(&svc, "/deployments").iter().find(|d| s(d, "/status") == "PRIMARY").cloned().unwrap_or_default();
    let (num, den, pct) = progress(&sp);
    let rs = format!("{} {}", p.rollout, s(&pdep, "/rolloutStateReason")).trim().to_string();
    let head = match p.rollout.as_str() {
        "FAILED" => Health::Bad,
        "COMPLETED" if sp.old.is_empty() && num == den => Health::Ok,
        _ => Health::Warn,
    };
    t.note(head, format!("PROGRESS {num}/{den} new tasks running = {pct:.0}% of service desiredCount · {rs} · {} since PRIMARY created", fmt_dur(elapsed)));
    let gen_line = |label: &str, d: &Value| {
        format!(
            "{label} {}: running {} · desired {} · pending {} · failedTasks {} · {} {}",
            taskdef(&s(d, "/taskDefinition")), n(d, "/runningCount"), n(d, "/desiredCount"), n(d, "/pendingCount"),
            n(d, "/failedTasks"), s(d, "/rolloutState"), s(d, "/rolloutStateReason"),
        )
    };
    t.note(if n(&pdep, "/failedTasks") > 0 { Health::Bad } else { Health::Ok }, gen_line("PRIMARY (new)", &pdep));
    for d in arr(&svc, "/deployments").iter().filter(|d| s(d, "/status") == "ACTIVE") {
        t.note(if n(d, "/failedTasks") > 0 { Health::Bad } else { Health::Warn }, gen_line("ACTIVE  (old)", d));
    }
    t.info(format!(
        "service desiredCount {} · runningCount {} is BOTH generations summed, not progress · pending {}",
        sp.desired, sp.running_all, sp.pending_all,
    ));
    if tgs.is_empty() && !arr(&svc, "/loadBalancers").is_empty() {
        t.bad("target health unavailable (see errors above)");
    }
    for (name, c) in &tgs {
        let g = |k: &str| c.get(k).copied().unwrap_or(0);
        let h = if g("unhealthy") > 0 { Health::Bad } else if g("draining") + g("initial") > 0 { Health::Warn } else { Health::Ok };
        let mut line = format!("target group {name}: healthy {} · unhealthy {} · draining {} · initial {}", g("healthy"), g("unhealthy"), g("draining"), g("initial"));
        for k in ["unused", "unavailable"] {
            if g(k) > 0 {
                line += &format!(" · {k} {}", g(k));
            }
        }
        t.note(h, line);
    }
    let since = |ptr: &str, x: &Value| matches!((parse_ts(&x[ptr]), start), (Some(a), Some(b)) if a >= b);
    let started = tasks.iter().filter(|x| since("createdAt", x)).count();
    let stopped: Vec<&Value> = tasks.iter().filter(|x| s(x, "/lastStatus") == "STOPPED" && since("stoppedAt", x)).collect();
    let failed = stopped.iter().filter(|x| task_health(x) == Health::Bad).count();
    let mut kinds: Vec<(&str, usize)> = vec![];
    for x in &stopped {
        let k = stop_kind(x, &sp);
        match kinds.iter_mut().find(|(kk, _)| *kk == k) {
            Some((_, c)) => *c += 1,
            None => kinds.push((k, 1)),
        }
    }
    kinds.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    let kinds: Vec<String> = kinds.iter().map(|(k, c)| format!("{c} {k}")).collect();
    let lower = if elapsed > 50 * 60 { " (lower bound: ECS drops stopped tasks after ~1h)" } else { "" };
    t.note(
        if failed > 0 { Health::Bad } else { Health::None },
        format!("CHURN since rollout start: {started} started · {} stopped: {}{lower}", stopped.len(), if kinds.is_empty() { "none".into() } else { kinds.join(", ") }),
    );
    let shown: Vec<&Value> = tasks
        .iter()
        .filter(|x| s(x, "/startedBy") == p.id || (s(x, "/lastStatus") == "STOPPED" && since("stoppedAt", x)))
        .collect();
    for x in &shown {
        task_row(&mut t, x, &cx);
    }
    // Failures first: during a roll the red rows are the point.
    let rank = |h: Health| match h { Health::Bad => 0, Health::Warn => 1, Health::Ok | Health::None => 2, Health::Dim => 3 };
    t.rows.sort_by_key(|r| rank(r.health));
    for e in &cx.events {
        let m = s(e, "/message");
        t.panel.push((event_health(&m), format!("{:>6} ago  {}", age_at(e, "/createdAt"), short_event(&m))));
    }
    Ok(t)
}

fn describe_task(aws: &Aws, cluster: &str, task: &str) -> Result<Value, AwsError> {
    let mut v = aws.call("ecs", "describe-tasks", &args!["--cluster", cluster, "--tasks", task])?;
    redact(&mut v);
    if let Some(f) = arr(&v, "/failures").first() {
        return Err(AwsError::Api(format!("describe-tasks: {} {}", s(f, "/arn"), s(f, "/reason"))));
    }
    arr(&v, "/tasks").first().cloned().ok_or_else(|| AwsError::Api(format!("task {task} not found (ECS keeps stopped tasks for about 1 hour)")))
}

fn containers(aws: &Aws, cluster: &str, task: &str) -> Result<Table, AwsError> {
    let x = describe_task(aws, cluster, task)?;
    let mut t = Table::new(&["NAME", "IMAGE TAG", "LAST", "HEALTH", "EXIT", "CPU", "MEM", "REASON"]);
    if !s(&x, "/stoppedReason").is_empty() {
        t.note(task_health(&x), format!("task stopped: {}: {}", s(&x, "/stopCode"), s(&x, "/stoppedReason")));
    }
    t.info("l: this container's logs · f: live tail");
    for c in arr(&x, "/containers") {
        let last = s(c, "/lastStatus");
        let h = if c.get("exitCode").is_some_and(|e| !e.is_null()) && n(c, "/exitCode") != 0 || s(c, "/healthStatus") == "UNHEALTHY" {
            Health::Bad
        } else {
            health_of(&last)
        };
        let name = s(c, "/name");
        t.row(name.clone(), h, c.clone(), vec![
            name,
            image_tag(&s(c, "/image")).to_string(),
            last,
            s(c, "/healthStatus"),
            s(c, "/exitCode"),
            Some(s(c, "/cpu")).filter(|x| !x.is_empty() && x != "0").unwrap_or_else(|| "-".into()),
            Some(s(c, "/memory")).filter(|x| !x.is_empty()).unwrap_or_else(|| "-".into()),
            s(c, "/reason"),
        ]);
    }
    Ok(t)
}

/// A task definition revision is immutable once registered, so each one is fetched once per run.
fn describe_taskdef(aws: &Aws, arn: &str) -> Result<Value, AwsError> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    // Only full ARNs (family:revision) are immutable; a bare family name means "latest".
    let key = (arn.contains(":task-definition/") || arn.rsplit_once(':').is_some_and(|x| x.1.parse::<u64>().is_ok()))
        .then(|| format!("{}/{}/{arn}", aws.ctx.profile, aws.ctx.region));
    if let Some(v) = key.as_ref().and_then(|k| cache.lock().unwrap().get(k).cloned()) {
        return Ok(v);
    }
    let mut v = aws.call("ecs", "describe-task-definition", &args!["--task-definition", arn])?;
    redact(&mut v);
    let td = v["taskDefinition"].clone();
    if let Some(k) = key {
        cache.lock().unwrap().insert(k, td.clone());
    }
    Ok(td)
}

fn taskdef_revisions(aws: &Aws, family: &str) -> Result<Table, AwsError> {
    // ACTIVE revisions can still be deployed; INACTIVE ones were deregistered and are the deploy history.
    let lists = par_map(&["ACTIVE", "INACTIVE"], |st| aws.call("ecs", "list-task-definitions", &args!["--family-prefix", family, "--status", st, "--sort", "DESC"]));
    let mut arns = vec![];
    for l in lists {
        arns.extend(strs(&l?, "/taskDefinitionArns"));
    }
    arns.retain(|a| taskdef(a).rsplit_once(':').map(|x| x.0) == Some(family));
    let rev_no = |a: &String| taskdef(a).rsplit_once(':').and_then(|x| x.1.parse::<u64>().ok()).unwrap_or(0);
    arns.sort_by_key(|a| std::cmp::Reverse(rev_no(a)));
    let mut t = Table::new(&["REVISION", "IMAGES", "CPU", "MEM", "REGISTERED", "STATUS"]);
    t.info("each deploy registers a new immutable revision; INACTIVE ones were deregistered (the history), ACTIVE ones can still run");
    let detailed = 10.min(arns.len());
    if arns.len() > detailed {
        t.info(format!("details loaded for the newest {detailed} of {} revisions; Enter loads any revision", arns.len()));
    }
    let details = par_map(&arns[..detailed], |a| describe_taskdef(aws, a));
    for (i, arn) in arns.iter().enumerate() {
        let rev = taskdef(arn).to_string();
        let d = match details.get(i) {
            Some(r) => t.soft(r.clone(), &format!("describe-task-definition {rev}"))?,
            None => None,
        };
        match d {
            Some(d) => {
                let imgs: Vec<String> = arr(&d, "/containerDefinitions").iter().map(|c| image_tag(&s(c, "/image")).to_string()).collect();
                t.row(arn.clone(), health_of(&s(&d, "/status")), d.clone(), vec![
                    rev,
                    imgs.join(","),
                    s(&d, "/cpu"),
                    s(&d, "/memory"),
                    age_at(&d, "/registeredAt"),
                    s(&d, "/status"),
                ]);
            }
            None => t.row(arn.clone(), Health::None, json!({ "taskDefinitionArn": arn }), vec![rev, "".into(), "".into(), "".into(), "".into(), "".into()]),
        }
    }
    Ok(t)
}

fn taskdef_containers(aws: &Aws, arn: &str) -> Result<Table, AwsError> {
    let d = describe_taskdef(aws, arn)?;
    let mut t = Table::new(&["NAME", "IMAGE TAG", "CPU", "MEM", "ESSENTIAL", "PORTS", "HEALTH CHECK", "LOG GROUP", "ENV", "SECRETS"]);
    let compat: Vec<String> = arr(&d, "/requiresCompatibilities").iter().filter_map(|x| x.as_str().map(String::from)).collect();
    t.info(format!(
        "task cpu {} · mem {} MiB · network {} · runs on {} · d shows a container's full definition; env and secret values are never shown",
        s(&d, "/cpu"), s(&d, "/memory"), s(&d, "/networkMode"), compat.join(", "),
    ));
    for c in arr(&d, "/containerDefinitions") {
        let ports: Vec<String> = arr(c, "/portMappings").iter().map(|p| format!("{}/{}", s(p, "/containerPort"), s(p, "/protocol"))).collect();
        let name = s(c, "/name");
        t.row(name.clone(), Health::None, c.clone(), vec![
            name,
            image_tag(&s(c, "/image")).to_string(),
            Some(s(c, "/cpu")).filter(|x| !x.is_empty() && x != "0").unwrap_or_else(|| "-".into()),
            Some(s(c, "/memory")).filter(|x| !x.is_empty()).unwrap_or_else(|| "-".into()),
            s(c, "/essential"),
            ports.join(","),
            if c.get("healthCheck").is_some() { "yes".into() } else { "-".into() },
            s(c, "/logConfiguration/options/awslogs-group"),
            arr(c, "/environment").len().to_string(),
            arr(c, "/secrets").len().to_string(),
        ]);
    }
    Ok(t)
}

pub fn fmt_bytes(b: i64) -> String {
    let b = b as f64;
    match () {
        _ if b >= 1e12 => format!("{:.1} TB", b / 1e12),
        _ if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        _ if b >= 1e6 => format!("{:.0} MB", b / 1e6),
        _ if b >= 1e3 => format!("{:.0} KB", b / 1e3),
        _ => format!("{b} B"),
    }
}

// ---------- logs ----------

fn log_groups(aws: &Aws) -> Result<Table, AwsError> {
    let v = aws.call("logs", "describe-log-groups", &[])?;
    let mut t = Table::new(&["NAME", "RETENTION", "STORED", "AGE"]);
    t.trunc(&v, "describe-log-groups");
    t.info("a log group is where a service's logs go; RETENTION never means kept, and billed, forever");
    for g in arr(&v, "/logGroups") {
        let name = s(g, "/logGroupName");
        let ret = g.get("retentionInDays").map(|r| format!("{r}d")).unwrap_or_else(|| "never expires".into());
        t.row(name.clone(), Health::None, g.clone(), vec![name, ret, fmt_bytes(n(g, "/storedBytes")), age_at(g, "/creationTime")]);
    }
    Ok(t)
}

/// awslogs stream name: <prefix>/<container>/<task-id>.
fn awslogs(c: &Value) -> Option<(String, String)> {
    (s(c, "/logConfiguration/logDriver") == "awslogs").then(|| {
        (s(c, "/logConfiguration/options/awslogs-group"), s(c, "/logConfiguration/options/awslogs-stream-prefix"))
    })
}

fn resolve(aws: &Aws, src: &LogSrc, pattern: &str, all: bool, t: &mut Table) -> Result<LogSel, AwsError> {
    let sel = |group: String, streams: Vec<String>, prefix: Option<String>| LogSel { group, streams, prefix, pattern: pattern.to_string(), containers: false };
    match src {
        LogSrc::Group(g) => Ok(sel(g.clone(), vec![], None)),
        LogSrc::Stream { group, stream } => Ok(sel(group.clone(), vec![stream.clone()], None)),
        LogSrc::Service { cluster, service } => {
            // The running tasks are listed while the service and task definition load.
            let list = || aws.call("ecs", "list-tasks", &args!["--cluster", cluster, "--service-name", service, "--desired-status", "RUNNING"]);
            let (svcs, running) = std::thread::scope(|sc| {
                let h = sc.spawn(list);
                (describe_services(aws, cluster, &[service.clone()], t), h.join().expect("list-tasks panicked"))
            });
            let svcs = svcs?;
            let svc = svcs.first().ok_or_else(|| AwsError::Api(format!("service {service} not found")))?;
            // Logs of the new generation: the PRIMARY deployment's task definition.
            let td_arn = arr(svc, "/deployments")
                .iter()
                .find(|d| s(d, "/status") == "PRIMARY")
                .map(|d| s(d, "/taskDefinition"))
                .unwrap_or_else(|| s(svc, "/taskDefinition"));
            let td = describe_taskdef(aws, &td_arn)?;
            let defs = arr(&td, "/containerDefinitions");
            let pick = defs
                .iter()
                .filter(|c| awslogs(c).is_some())
                .max_by_key(|c| c["essential"].as_bool().unwrap_or(true))
                .ok_or_else(|| AwsError::Api(format!("{} has no awslogs container", taskdef(&td_arn))))?;
            let (group, prefix) = awslogs(pick).unwrap();
            // With `a`, every container logging to the same group and prefix (k9s AllContainers).
            let names: Vec<String> = if all {
                defs.iter().filter(|c| awslogs(c) == Some((group.clone(), prefix.clone()))).map(|c| s(c, "/name")).collect()
            } else {
                vec![s(pick, "/name")]
            };
            let others = defs.iter().filter(|c| awslogs(c).is_some()).count();
            if !all && others > 1 {
                t.info(format!("showing container `{}` · a shows all {others} containers", s(pick, "/name")));
            }
            if all && names.len() == 1 {
                t.info(format!("`{}` is the only container logging to {group}", names[0]));
            }
            if prefix.is_empty() {
                t.bad("no awslogs-stream-prefix: showing the whole log group, which may mix services");
                return Ok(sel(group, vec![], None));
            }
            // Name the running tasks' streams (k9s: the pods' logs). The stream prefix alone also
            // matches every stream the service ever had, thousands, which makes each read slow.
            let ids: Vec<String> = strs(&running?, "/taskArns").iter().map(|a| short(a).to_string()).collect();
            let p = &prefix;
            let streams: Vec<String> = ids.iter().flat_map(|id| names.iter().map(move |n| format!("{p}/{n}/{id}"))).collect();
            if streams.is_empty() {
                t.info("no running tasks: showing every stream of the service, including stopped tasks");
                let only = if names.len() == 1 { format!("{prefix}/{}/", names[0]) } else { format!("{prefix}/") };
                return Ok(LogSel { containers: names.len() > 1, ..sel(group, vec![], Some(only)) });
            }
            Ok(LogSel { containers: names.len() > 1, ..sel(group, streams, None) })
        }
        LogSrc::Task { cluster, task, container } => {
            let x = describe_task(aws, cluster, task)?;
            let td = describe_taskdef(aws, &s(&x, "/taskDefinitionArn"))?;
            let id = short(task).to_string();
            let mut by_group: Vec<(String, Vec<String>)> = vec![];
            for c in arr(&td, "/containerDefinitions") {
                let name = s(c, "/name");
                if container.as_ref().is_some_and(|want| *want != name) {
                    continue;
                }
                let Some((g, p)) = awslogs(c) else { continue };
                let stream = if p.is_empty() { id.clone() } else { format!("{p}/{name}/{id}") };
                match by_group.iter_mut().find(|(gg, _)| *gg == g) {
                    Some((_, v)) => v.push(stream),
                    None => by_group.push((g, vec![stream])),
                }
            }
            if by_group.len() > 1 {
                t.info("containers log to several groups; showing the first. Use the containers view (Enter) for the others");
            }
            let (g, streams) = by_group.into_iter().next().ok_or_else(|| AwsError::Api(match container {
                Some(c) => format!("container `{c}` has no awslogs log configuration, so it has no CloudWatch logs"),
                None => "no container in this task logs to CloudWatch (awslogs)".into(),
            }))?;
            Ok(LogSel { containers: streams.len() > 1, ..sel(g, streams, None) })
        }
    }
}

fn logs(aws: &Aws, src: &LogSrc, pattern: &str, opts: LogOpts) -> Result<Table, AwsError> {
    let mut t = Table::new(&["TIME", "STREAM", "MESSAGE"]);
    let sel = resolve(aws, src, pattern, opts.all, &mut t)?;
    let now = Utc::now().timestamp_millis();
    let fetch = |from_ms: i64, cap: usize| filter_all(aws, &sel, from_ms, cap).map(|(evs, cut, rate)| {
        let mut v = json!({ "events": evs, "rate": rate });
        if cut {
            v["nextToken"] = json!("more");
        }
        v
    });
    let between = |evs: &[Value]| match (evs.first(), evs.last()) {
        (Some(a), Some(b)) => format!("{} to {}", fmt_time(a), fmt_time(b)),
        _ => String::new(),
    };
    let one_stream = sel.streams.len() == 1 && pattern.is_empty();
    let keys = "0 tail · 1 head · 2-6 1m 5m 15m 30m 1h · / filters · p server-side pattern";
    let matching = if pattern.is_empty() { String::new() } else { format!(" matching `{pattern}`") };
    let got = match opts.since {
        Since::Head => {
            // The first lines: each stream from its start (a task's logs begin when it started).
            let v = if one_stream {
                aws.call("logs", "get-log-events", &args!["--log-group-name", sel.group, "--log-stream-name", sel.streams[0], "--start-from-head", "--limit", LOG_MAX_ITEMS])?
            } else {
                fetch(0, LOG_MAX_ITEMS)?
            };
            let evs = arr(&v, "/events");
            t.info(format!("{} · {} · head: the first {} lines ({}) · not following (s follows from the newest line shown) · {keys}", sel.group, streams_note(&sel), evs.len(), between(evs)));
            t.empty = Some(format!("no log events{matching}"));
            v
        }
        Since::Secs(w) => {
            let v = fetch(now - w * 1000, LOG_SINCE_MAX_ITEMS)?;
            let evs = arr(&v, "/events");
            let span = fmt_dur(w);
            if truncated(&v) {
                t.bad(format!(
                    "the last {span} holds more than one read returns ({LOG_SINCE_MAX_ITEMS} lines per 100 streams): showing the first {} ({}), then following new lines. Narrow with p (server-side pattern) or a shorter window",
                    evs.len(), between(evs),
                ));
            } else {
                t.info(format!("{} · {} · last {span}: {} events · following new lines (s pauses) · {keys}", sel.group, streams_note(&sel), evs.len()));
            }
            t.empty = Some(format!("no log events{matching} in the last {span}"));
            v
        }
        // CloudWatch returns the oldest events of a window first. To show the newest lines:
        // one stream asks get-log-events for its tail directly; several streams take one probe call to
        // measure the rate, then fetch just the last few seconds' worth. Quiet sources widen to 15m.
        Since::Tail if one_stream => {
            let v = aws.call("logs", "get-log-events", &args![
                "--log-group-name", sel.group, "--log-stream-name", sel.streams[0], "--no-start-from-head", "--limit", LOG_MAX_ITEMS,
            ])?;
            t.info(format!("{} · newest {} lines of this stream · following new lines (s pauses) · {keys}", sel.group, arr(&v, "/events").len()));
            t.empty = Some("this stream has no log events".into());
            v
        }
        Since::Tail => {
            let probe = fetch(now - 60_000, LOG_MAX_ITEMS)?;
            let evs = arr(&probe, "/events");
            let (v, span) = if truncated(&probe) && evs.len() > 1 {
                let per_sec = probe["rate"].as_f64().unwrap_or(1.0).max(0.1);
                // Seconds that hold ~80% of the cap, plus 2s for ingestion delay.
                let w = ((LOG_MAX_ITEMS as f64 * 0.8) / per_sec).ceil() as i64 + 2;
                let v = fetch(now - w * 1000, LOG_MAX_ITEMS)?;
                let evs = arr(&v, "/events");
                t.info(format!(
                    "{} · {} · busy: about {per_sec:.0} lines/s, started from the most recent {} ({}) · following new lines (s pauses) · {keys}",
                    sel.group, streams_note(&sel), evs.len(), between(evs),
                ));
                (v, w)
            } else {
                let (v, w) = if evs.len() < 200 {
                    let wide = fetch(now - 900_000, LOG_MAX_ITEMS)?;
                    if truncated(&wide) { (probe, 60) } else { (wide, 900) }
                } else {
                    (probe, 60)
                };
                t.info(format!("{} · {} · last {}: {} events · following new lines (s pauses) · {keys}", sel.group, streams_note(&sel), fmt_dur(w), arr(&v, "/events").len()));
                (v, w)
            };
            t.empty = Some(format!("no log events{matching} in the last {}", fmt_dur(span)));
            v
        }
    };
    t.rows.extend(arr(&got, "/events").iter().map(|e| log_row(e, &sel)));
    t.tail = Some(sel);
    Ok(t)
}

fn streams_note(sel: &LogSel) -> String {
    match sel.streams.len() {
        0 => "all streams".into(),
        1 => "1 stream".into(),
        k if sel.containers => format!("{k} streams (task and container), merged by time"),
        k => format!("{k} tasks, merged by time"),
    }
}

/// filter-log-events takes at most 100 stream names per call.
const MAX_LOG_STREAMS: usize = 100;

/// filter-log-events arguments for a selection, one call per 100 streams, without time bounds.
fn filter_calls(sel: &LogSel) -> Vec<Vec<String>> {
    let base = |streams: &[String]| {
        let mut a = args!["--log-group-name", sel.group];
        if !streams.is_empty() {
            a.push("--log-stream-names".into());
            a.extend(streams.iter().cloned());
        } else if let Some(p) = &sel.prefix {
            a.extend(args!["--log-stream-name-prefix", p]);
        }
        if !sel.pattern.is_empty() {
            a.extend(args!["--filter-pattern", sel.pattern]);
        }
        a
    };
    if sel.streams.is_empty() {
        return vec![base(&[])];
    }
    sel.streams.chunks(MAX_LOG_STREAMS).map(base).collect()
}

/// Every call of a selection in parallel, merged by time. Returns the events, whether any call
/// was cut at `max` (each call's cap is its own), and the combined lines per second.
fn filter_all(aws: &Aws, sel: &LogSel, start_ms: i64, max: usize) -> Result<(Vec<Value>, bool, f64), AwsError> {
    let secs = ((Utc::now().timestamp_millis() - start_ms) as f64 / 1000.0).max(1.0);
    let calls = filter_calls(sel);
    let got = par_map(&calls, |a| {
        let mut a = a.clone();
        a.extend(args!["--start-time", start_ms, "--max-items", max]);
        aws.call("logs", "filter-log-events", &a)
    });
    let (mut evs, mut cut, mut rate) = (vec![], false, 0.0);
    for v in got {
        let v = v?;
        let e = arr(&v, "/events");
        if truncated(&v) && e.len() > 1 {
            cut = true;
            let span_ms = (n(e.last().unwrap(), "/timestamp") - n(&e[0], "/timestamp")).max(1) as f64;
            rate += e.len() as f64 * 1000.0 / span_ms;
        } else {
            cut |= truncated(&v);
            rate += e.len() as f64 / secs;
        }
        evs.extend(e.iter().cloned());
    }
    evs.sort_by_key(|e| n(e, "/timestamp"));
    Ok((evs, cut, rate))
}

/// One log line; the STREAM cell names the task it came from (and the container, when several log),
/// empty for a single stream.
pub fn log_row(e: &Value, sel: &LogSel) -> Row {
    let name = s(e, "/logStreamName");
    let stream = match name.rsplit('/').nth(1) {
        _ if sel.streams.len() == 1 => String::new(),
        Some(container) if sel.containers => format!("{container}/{}", stream_tail(&name)),
        _ => stream_tail(&name),
    };
    Row { id: s(e, "/eventId"), health: Health::None, json: e.clone(), cells: vec![fmt_time(e), stream, s(e, "/message").trim_end().to_string()] }
}

/// Seconds of overlap each tail poll re-reads, for lines CloudWatch ingests late or out of order.
const TAIL_OVERLAP_MS: i64 = 5_000;
const TAIL_MAX_ITEMS: usize = 10_000;

/// One live-tail poll: the lines after `cursor`, deduplicated against `seen` (event id to timestamp).
/// Polls filter-log-events instead of StartLiveTail because the CLI's live tail prints bare messages,
/// with no stream name to say which task a line came from. Returns the new rows and whether the
/// poll hit TAIL_MAX_ITEMS (lines were skipped).
pub fn tail_poll(aws: &Aws, sel: &LogSel, cursor: &mut i64, seen: &mut HashMap<String, i64>) -> Result<(Vec<Row>, bool), AwsError> {
    let (evs, skipped, _) = filter_all(aws, sel, *cursor - TAIL_OVERLAP_MS, TAIL_MAX_ITEMS)?;
    let evs: Vec<&Value> = evs.iter().filter(|e| seen.insert(s(e, "/eventId"), n(e, "/timestamp")).is_none()).collect();
    *cursor = evs.iter().map(|e| n(e, "/timestamp")).fold(*cursor, i64::max);
    if skipped {
        // Falling behind: jump to now rather than drift further back.
        *cursor = (*cursor).max(Utc::now().timestamp_millis());
    }
    let cut = *cursor - 2 * TAIL_OVERLAP_MS;
    seen.retain(|_, ts| *ts >= cut);
    Ok((evs.into_iter().map(|e| log_row(e, sel)).collect(), skipped))
}

fn fmt_time(e: &Value) -> String {
    parse_ts(&e["timestamp"]).map(|d| d.with_timezone(&Local).format("%H:%M:%S%.3f").to_string()).unwrap_or_default()
}

/// The task id a stream belongs to, at the 12 characters the task list shows.
pub fn stream_tail(stream: &str) -> String {
    short(stream).chars().take(12).collect()
}

// ---------- ELBv2 ----------

fn load_balancers(aws: &Aws) -> Result<Table, AwsError> {
    let v = aws.call("elbv2", "describe-load-balancers", &[])?;
    let mut t = Table::new(&["NAME", "TYPE", "SCHEME", "STATE", "DNS", "AGE"]);
    for lb in arr(&v, "/LoadBalancers") {
        let state = s(lb, "/State/Code");
        t.row(s(lb, "/LoadBalancerArn"), health_of(&state), lb.clone(), vec![
            s(lb, "/LoadBalancerName"),
            s(lb, "/Type"),
            s(lb, "/Scheme"),
            state,
            s(lb, "/DNSName"),
            age_at(lb, "/CreatedTime"),
        ]);
    }
    Ok(t)
}

fn attrs(v: &Value) -> HashMap<String, String> {
    arr(v, "/Attributes").iter().map(|a| (s(a, "/Key"), s(a, "/Value"))).collect()
}

fn target_groups(aws: &Aws, lb: Option<&str>) -> Result<Table, AwsError> {
    let a = lb.map(|l| args!["--load-balancer-arn", l]).unwrap_or_default();
    let v = aws.call("elbv2", "describe-target-groups", &a)?;
    let tgs = arr(&v, "/TargetGroups");
    let mut t = Table::new(&["NAME", "PROTO:PORT", "TYPE", "HEALTHY", "OTHER STATES", "ALGORITHM", "SLOW START", "DEREG DELAY", "LBS"]);
    let extra = par_map(tgs, |tg| {
        let arn = s(tg, "/TargetGroupArn");
        (
            aws.call("elbv2", "describe-target-health", &args!["--target-group-arn", arn]),
            aws.call("elbv2", "describe-target-group-attributes", &args!["--target-group-arn", arn]),
        )
    });
    for (tg, (health, at)) in tgs.iter().zip(extra) {
        let name = s(tg, "/TargetGroupName");
        let states: Vec<String> = t
            .soft(health, &format!("describe-target-health {name}"))?
            .map(|h| arr(&h, "/TargetHealthDescriptions").iter().map(|d| s(d, "/TargetHealth/State")).collect())
            .unwrap_or_default();
        let at = t.soft(at, &format!("describe-target-group-attributes {name}"))?.map(|a| attrs(&a)).unwrap_or_default();
        let healthy = states.iter().filter(|x| *x == "healthy").count();
        let mut other: HashMap<&str, usize> = HashMap::new();
        for st in states.iter().filter(|x| *x != "healthy") {
            *other.entry(st.as_str()).or_default() += 1;
        }
        let mut other: Vec<String> = other.into_iter().map(|(k, v)| format!("{k} {v}")).collect();
        other.sort();
        let h = if states.iter().any(|x| x == "unhealthy") {
            Health::Bad
        } else if states.is_empty() || healthy < states.len() {
            Health::Warn
        } else {
            Health::Ok
        };
        let mut j = tg.clone();
        j["Attributes"] = json!(at);
        t.row(s(tg, "/TargetGroupArn"), h, j, vec![
            name,
            format!("{}:{}", s(tg, "/Protocol"), s(tg, "/Port")),
            s(tg, "/TargetType"),
            format!("{healthy}/{}", states.len()),
            other.join(" · "),
            at.get("load_balancing.algorithm.type").cloned().unwrap_or_default(),
            at.get("slow_start.duration_seconds").map(|x| format!("{x}s")).unwrap_or_default(),
            at.get("deregistration_delay.timeout_seconds").map(|x| format!("{x}s")).unwrap_or_default(),
            arr(tg, "/LoadBalancerArns").len().to_string(),
        ]);
    }
    Ok(t)
}

fn targets(aws: &Aws, arn: &str) -> Result<Table, AwsError> {
    let v = aws.call("elbv2", "describe-target-health", &args!["--target-group-arn", arn])?;
    let mut t = Table::new(&["TARGET", "PORT", "AZ", "STATE", "REASON", "DESCRIPTION"]);
    if let Some(a) = t.soft(aws.call("elbv2", "describe-target-group-attributes", &args!["--target-group-arn", arn]), "attributes")? {
        let a = attrs(&a);
        let get = |k: &str| a.get(k).cloned().unwrap_or_else(|| "-".into());
        t.info(format!(
            "algorithm {} · slow_start {}s · deregistration_delay {}s · stickiness {}",
            get("load_balancing.algorithm.type"),
            get("slow_start.duration_seconds"),
            get("deregistration_delay.timeout_seconds"),
            get("stickiness.enabled"),
        ));
    }
    for d in arr(&v, "/TargetHealthDescriptions") {
        let state = s(d, "/TargetHealth/State");
        t.row(format!("{}:{}", s(d, "/Target/Id"), s(d, "/Target/Port")), health_of(&state), d.clone(), vec![
            s(d, "/Target/Id"),
            s(d, "/Target/Port"),
            s(d, "/Target/AvailabilityZone"),
            state,
            s(d, "/TargetHealth/Reason"),
            s(d, "/TargetHealth/Description"),
        ]);
    }
    Ok(t)
}

// ---------- Application Auto Scaling ----------

fn scalable_targets(aws: &Aws, ns: &str) -> Result<Table, AwsError> {
    let v = aws.call("application-autoscaling", "describe-scalable-targets", &args!["--service-namespace", ns])?;
    let mut t = Table::new(&["RESOURCE", "SCALES", "MIN", "MAX", "SUSPENDED", "AGE"]);
    t.info("autoscaling keeps each resource between MIN and MAX (Kubernetes: an HPA) · Enter: what it did · p: its rules");
    for x in arr(&v, "/ScalableTargets") {
        let susp: Vec<&str> = x
            .get("SuspendedState")
            .and_then(Value::as_object)
            .map(|o| o.iter().filter(|(_, v)| v.as_bool() == Some(true)).map(|(k, _)| k.as_str()).collect())
            .unwrap_or_default();
        t.row(s(x, "/ResourceId"), if susp.is_empty() { Health::None } else { Health::Warn }, x.clone(), vec![
            s(x, "/ResourceId").trim_start_matches("service/").to_string(),
            short(&s(x, "/ScalableDimension")).to_string(),
            s(x, "/MinCapacity"),
            s(x, "/MaxCapacity"),
            susp.join(","),
            age_at(x, "/CreationTime"),
        ]);
    }
    Ok(t)
}

fn scaling_policies(aws: &Aws, ns: &str, resource: Option<&str>) -> Result<Table, AwsError> {
    let mut a = args!["--service-namespace", ns];
    if let Some(r) = resource {
        a.extend(args!["--resource-id", r]);
    }
    let v = aws.call("application-autoscaling", "describe-scaling-policies", &a)?;
    let mut t = Table::new(&["NAME", "RESOURCE", "TYPE", "TARGET", "METRIC", "COOLDOWN IN/OUT", "WHAT IT DOES"]);
    for p in arr(&v, "/ScalingPolicies") {
        let c = &p["TargetTrackingScalingPolicyConfiguration"];
        let metric = [s(c, "/PredefinedMetricSpecification/PredefinedMetricType"), s(c, "/CustomizedMetricSpecification/MetricName")].concat();
        let human = match metric.as_str() {
            "ECSServiceAverageCPUUtilization" => "average CPU %".to_string(),
            "ECSServiceAverageMemoryUtilization" => "average memory %".to_string(),
            "ALBRequestCountPerTarget" => "requests per target".to_string(),
            m => m.to_string(),
        };
        let what = match s(p, "/PolicyType").as_str() {
            "TargetTrackingScaling" => format!(
                "keeps {human} near {}: adds tasks when above (at most every {}s), removes them when below (at most every {}s){}. Kubernetes: an HPA with that target",
                s(c, "/TargetValue"),
                s(c, "/ScaleOutCooldown"),
                s(c, "/ScaleInCooldown"),
                if c["DisableScaleIn"].as_bool() == Some(true) { "; scale-in is disabled" } else { "" },
            ),
            "StepScaling" => "changes the count in steps when its CloudWatch alarm fires".to_string(),
            other => other.to_string(),
        };
        t.row(s(p, "/PolicyARN"), Health::None, p.clone(), vec![
            s(p, "/PolicyName"),
            s(p, "/ResourceId").trim_start_matches("service/").to_string(),
            s(p, "/PolicyType"),
            s(c, "/TargetValue"),
            metric,
            format!("{}/{}", s(c, "/ScaleInCooldown"), s(c, "/ScaleOutCooldown")),
            what,
        ]);
    }
    t.empty = Some("no scaling policies: the count only changes when someone sets it".into());
    Ok(t)
}

fn scaling_activities(aws: &Aws, ns: &str, resource: Option<&str>) -> Result<Table, AwsError> {
    let mut a = args!["--service-namespace", ns, "--max-items", 200];
    if let Some(r) = resource {
        a.extend(args!["--resource-id", r]);
    }
    let v = aws.call("application-autoscaling", "describe-scaling-activities", &a)?;
    let mut t = Table::new(&["AGE", "STATUS", "RESOURCE", "DESCRIPTION", "CAUSE"]);
    if truncated(&v) {
        t.info("showing the 200 most recent activities; older ones exist");
    }
    for x in arr(&v, "/ScalingActivities") {
        let st = s(x, "/StatusCode");
        let msg = s(x, "/StatusMessage");
        t.row(s(x, "/ActivityId"), health_of(&st), x.clone(), vec![
            age_at(x, "/StartTime"),
            st,
            s(x, "/ResourceId"),
            s(x, "/Description"),
            if msg.is_empty() { s(x, "/Cause") } else { msg },
        ]);
    }
    t.empty = Some("no scaling activity recorded here; AWS keeps the last six weeks".into());
    Ok(t)
}

// ---------- CloudWatch metrics ----------

struct Q {
    label: String,
    ns: &'static str,
    metric: &'static str,
    dims: Vec<(&'static str, String)>,
    stat: &'static str,
}

fn q(label: &str, ns: &'static str, metric: &'static str, dims: &[(&'static str, String)], stat: &'static str) -> Q {
    Q { label: label.into(), ns, metric, dims: dims.to_vec(), stat }
}

/// One GetMetricData call; values ascending per query, in query order.
fn metric_data(aws: &Aws, qs: &[Q], minutes: i64, period: i64) -> Result<Vec<Vec<f64>>, AwsError> {
    let queries: Vec<Value> = qs
        .iter()
        .enumerate()
        .map(|(i, x)| json!({
            "Id": format!("m{i}"),
            "Label": x.label,
            "MetricStat": {
                "Metric": {"Namespace": x.ns, "MetricName": x.metric,
                           "Dimensions": x.dims.iter().map(|(k, v)| json!({"Name": k, "Value": v})).collect::<Vec<_>>()},
                "Period": period, "Stat": x.stat},
            "ReturnData": true,
        }))
        .collect();
    let end = Utc::now();
    let start = end - chrono::Duration::minutes(minutes);
    let v = aws.call("cloudwatch", "get-metric-data", &args![
        "--metric-data-queries", Value::Array(queries),
        "--start-time", start.to_rfc3339(), "--end-time", end.to_rfc3339(),
        "--scan-by", "TimestampAscending",
    ])?;
    let mut by_id: HashMap<String, Vec<f64>> = HashMap::new();
    for r in arr(&v, "/MetricDataResults") {
        by_id.entry(s(r, "/Id")).or_default().extend(arr(r, "/Values").iter().filter_map(Value::as_f64));
    }
    Ok((0..qs.len()).map(|i| by_id.remove(&format!("m{i}")).unwrap_or_default()).collect())
}

fn fmt_num(f: f64) -> String {
    if (f - f.round()).abs() < 1e-9 { format!("{}", f as i64) } else { format!("{f:.2}") }
}

fn metrics(aws: &Aws, target: &MetricTarget) -> Result<Table, AwsError> {
    let mut t = Table::new(&["METRIC", "STAT", "LATEST", "MIN", "MAX", "LAST 3H (5m)"]);
    let dim = |arn: &str, key: &str| arn.split_once(key).map(|x| x.1.to_string()).unwrap_or_default();
    let qs = match target {
        MetricTarget::Service { cluster, service } => {
            let d = [("ClusterName", cluster.clone()), ("ServiceName", service.clone())];
            vec![
                q("CPUUtilization %", "AWS/ECS", "CPUUtilization", &d, "Average"),
                q("CPUUtilization %", "AWS/ECS", "CPUUtilization", &d, "Maximum"),
                q("MemoryUtilization %", "AWS/ECS", "MemoryUtilization", &d, "Average"),
                q("MemoryUtilization %", "AWS/ECS", "MemoryUtilization", &d, "Maximum"),
            ]
        }
        MetricTarget::Lb { arn } => {
            if arn.contains("/net/") {
                t.bad("network load balancer: only ALB metrics are covered");
            }
            let d = [("LoadBalancer", dim(arn, ":loadbalancer/"))];
            vec![
                q("RequestCount", "AWS/ApplicationELB", "RequestCount", &d, "Sum"),
                q("HTTPCode_ELB_5XX_Count", "AWS/ApplicationELB", "HTTPCode_ELB_5XX_Count", &d, "Sum"),
                q("HTTPCode_Target_5XX_Count", "AWS/ApplicationELB", "HTTPCode_Target_5XX_Count", &d, "Sum"),
                q("TargetResponseTime (s)", "AWS/ApplicationELB", "TargetResponseTime", &d, "p95"),
            ]
        }
        MetricTarget::Tg { arn, lb } => {
            let tg = ("TargetGroup", arn.rsplit(':').next().unwrap_or_default().to_string());
            let Some(lb) = lb else {
                t.bad("target group is not attached to a load balancer: no ALB metrics");
                return Ok(t);
            };
            let d = [tg, ("LoadBalancer", dim(lb, ":loadbalancer/"))];
            vec![
                q("RequestCountPerTarget", "AWS/ApplicationELB", "RequestCountPerTarget", &d[..1], "Sum"),
                q("HTTPCode_Target_5XX_Count", "AWS/ApplicationELB", "HTTPCode_Target_5XX_Count", &d, "Sum"),
                q("TargetResponseTime (s)", "AWS/ApplicationELB", "TargetResponseTime", &d, "p95"),
                q("HealthyHostCount", "AWS/ApplicationELB", "HealthyHostCount", &d, "Minimum"),
                q("UnHealthyHostCount", "AWS/ApplicationELB", "UnHealthyHostCount", &d, "Maximum"),
            ]
        }
    };
    let data = metric_data(aws, &qs, 180, 300)?;
    for (x, vals) in qs.iter().zip(data) {
        let (lo, hi) = vals.iter().fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));
        let cells = if vals.is_empty() {
            vec![x.label.clone(), x.stat.into(), "no data".into(), "".into(), "".into(), "".into()]
        } else {
            vec![x.label.clone(), x.stat.into(), fmt_num(*vals.last().unwrap()), fmt_num(lo), fmt_num(hi), sparkline(&vals)]
        };
        t.row(format!("{} {}", x.label, x.stat), Health::None, json!({"label": x.label, "stat": x.stat, "values": vals}), cells);
    }
    Ok(t)
}

// ---------- EC2, ECR, SQS, RDS, Lambda ----------

fn ec2(aws: &Aws) -> Result<Table, AwsError> {
    let v = aws.call("ec2", "describe-instances", &[])?;
    let mut t = Table::new(&["ID", "NAME", "STATE", "TYPE", "AZ", "PRIVATE IP", "AGE"]);
    for r in arr(&v, "/Reservations") {
        for i in arr(r, "/Instances") {
            let name = arr(i, "/Tags").iter().find(|x| s(x, "/Key") == "Name").map(|x| s(x, "/Value")).unwrap_or_default();
            let state = s(i, "/State/Name");
            t.row(s(i, "/InstanceId"), health_of(&state), i.clone(), vec![
                s(i, "/InstanceId"),
                name,
                state,
                s(i, "/InstanceType"),
                s(i, "/Placement/AvailabilityZone"),
                s(i, "/PrivateIpAddress"),
                age_at(i, "/LaunchTime"),
            ]);
        }
    }
    Ok(t)
}

fn ecr_repos(aws: &Aws) -> Result<Table, AwsError> {
    let v = aws.call("ecr", "describe-repositories", &[])?;
    let mut t = Table::new(&["NAME", "URI", "TAGS", "SCAN ON PUSH", "AGE"]);
    for r in arr(&v, "/repositories") {
        let name = s(r, "/repositoryName");
        t.row(name.clone(), Health::None, r.clone(), vec![
            name,
            s(r, "/repositoryUri"),
            s(r, "/imageTagMutability"),
            s(r, "/imageScanningConfiguration/scanOnPush"),
            age_at(r, "/createdAt"),
        ]);
    }
    Ok(t)
}

fn ecr_images(aws: &Aws, repo: &str) -> Result<Table, AwsError> {
    let v = aws.call("ecr", "describe-images", &args!["--repository-name", repo])?;
    let mut imgs = arr(&v, "/imageDetails").to_vec();
    imgs.sort_by_key(|i| std::cmp::Reverse(parse_ts(&i["imagePushedAt"])));
    let mut t = Table::new(&["TAGS", "DIGEST", "SIZE MB", "PUSHED", "LAST PULL"]);
    for i in imgs {
        let digest = s(&i, "/imageDigest");
        t.row(digest.clone(), Health::None, i.clone(), vec![
            strs(&i, "/imageTags").join(","),
            digest.trim_start_matches("sha256:").chars().take(12).collect(),
            (n(&i, "/imageSizeInBytes") / 1_000_000).to_string(),
            age_at(&i, "/imagePushedAt"),
            age_at(&i, "/lastRecordedPullTime"),
        ]);
    }
    Ok(t)
}

fn sqs(aws: &Aws) -> Result<Table, AwsError> {
    let urls = strs(&aws.call("sqs", "list-queues", &[])?, "/QueueUrls");
    let mut t = Table::new(&["NAME", "VISIBLE", "IN FLIGHT", "DELAYED", "OLDEST", "DLQ", "RETENTION"]);
    let attrs = par_map(&urls, |u| aws.call("sqs", "get-queue-attributes", &args!["--queue-url", u, "--attribute-names", "All"]));
    let names: Vec<String> = urls.iter().map(|u| short(u).to_string()).collect();
    let mut oldest: Vec<Option<f64>> = vec![None; names.len()];
    for (ci, chunk) in names.chunks(500).enumerate() {
        let qs: Vec<Q> = chunk
            .iter()
            .map(|nm| q(nm, "AWS/SQS", "ApproximateAgeOfOldestMessage", &[("QueueName", nm.clone())], "Maximum"))
            .collect();
        match metric_data(aws, &qs, 15, 60) {
            Ok(d) => d.into_iter().enumerate().for_each(|(i, v)| oldest[ci * 500 + i] = v.last().copied()),
            Err(e @ AwsError::SsoExpired(_)) => return Err(e),
            Err(e) => t.bad(format!("oldest-message metric: {e}")),
        }
    }
    for ((name, a), old) in names.into_iter().zip(attrs).zip(oldest) {
        let Some(a) = t.soft(a, &format!("get-queue-attributes {name}"))? else { continue };
        let at = &a["Attributes"];
        let dlq = serde_json::from_str::<Value>(&s(at, "/RedrivePolicy")).map(|r| short(&s(&r, "/deadLetterTargetArn")).to_string()).unwrap_or_default();
        let h = match old {
            Some(x) if x > 900.0 => Health::Bad,
            Some(x) if x > 60.0 => Health::Warn,
            _ => Health::Ok,
        };
        t.row(name.clone(), h, a.clone(), vec![
            name,
            s(at, "/ApproximateNumberOfMessages"),
            s(at, "/ApproximateNumberOfMessagesNotVisible"),
            s(at, "/ApproximateNumberOfMessagesDelayed"),
            old.map(|x| fmt_dur(x as i64)).unwrap_or_else(|| "-".into()),
            dlq,
            fmt_dur(s(at, "/MessageRetentionPeriod").parse().unwrap_or(0)),
        ]);
    }
    Ok(t)
}

fn rds(aws: &Aws) -> Result<Table, AwsError> {
    let clusters = aws.call("rds", "describe-db-clusters", &[])?;
    let instances = aws.call("rds", "describe-db-instances", &[])?;
    let mut t = Table::new(&["KIND", "ID", "ENGINE", "CLASS", "STATUS", "ROLE", "AZ", "AGE"]);
    let mut writer: HashMap<String, bool> = HashMap::new();
    for c in arr(&clusters, "/DBClusters") {
        for m in arr(c, "/DBClusterMembers") {
            writer.insert(s(m, "/DBInstanceIdentifier"), m["IsClusterWriter"].as_bool().unwrap_or(false));
        }
        let st = s(c, "/Status");
        t.row(s(c, "/DBClusterArn"), health_of(&st), c.clone(), vec![
            "cluster".into(),
            s(c, "/DBClusterIdentifier"),
            format!("{} {}", s(c, "/Engine"), s(c, "/EngineVersion")),
            s(c, "/DBClusterInstanceClass"),
            st,
            format!("{} members", arr(c, "/DBClusterMembers").len()),
            if c["MultiAZ"].as_bool() == Some(true) { "multi-az".into() } else { String::new() },
            age_at(c, "/ClusterCreateTime"),
        ]);
    }
    for i in arr(&instances, "/DBInstances") {
        let id = s(i, "/DBInstanceIdentifier");
        let role = match writer.get(&id) {
            Some(true) => format!("writer ({})", s(i, "/DBClusterIdentifier")),
            Some(false) => format!("reader ({})", s(i, "/DBClusterIdentifier")),
            None if !s(i, "/ReadReplicaSourceDBInstanceIdentifier").is_empty() => "replica".into(),
            None => "standalone".into(),
        };
        let st = s(i, "/DBInstanceStatus");
        t.row(s(i, "/DBInstanceArn"), health_of(&st), i.clone(), vec![
            "instance".into(),
            id,
            format!("{} {}", s(i, "/Engine"), s(i, "/EngineVersion")),
            s(i, "/DBInstanceClass"),
            st,
            role,
            s(i, "/AvailabilityZone"),
            age_at(i, "/InstanceCreateTime"),
        ]);
    }
    Ok(t)
}

fn lambda(aws: &Aws) -> Result<Table, AwsError> {
    let mut v = aws.call("lambda", "list-functions", &[])?;
    redact(&mut v);
    let mut t = Table::new(&["NAME", "RUNTIME", "MEM", "TIMEOUT", "ARCH", "CODE MB", "MODIFIED"]);
    for f in arr(&v, "/Functions") {
        let name = s(f, "/FunctionName");
        t.row(name.clone(), Health::None, f.clone(), vec![
            name,
            s(f, "/Runtime"),
            s(f, "/MemorySize"),
            format!("{}s", s(f, "/Timeout")),
            strs(f, "/Architectures").join(","),
            format!("{:.1}", n(f, "/CodeSize") as f64 / 1e6),
            age_at(f, "/LastModified"),
        ]);
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// list-services returns `total` services; describe-services enforces the real 10-name limit
    /// and fails the chunk that contains `fail_name`.
    fn fake(total: usize, fail_name: Option<&'static str>, calls: Arc<Mutex<Vec<usize>>>) -> Aws {
        Aws::with_runner(move |_, svc, op, args| {
            assert_eq!(svc, "ecs");
            match op {
                "list-services" => Ok(json!({"serviceArns": (0..total).map(|i| format!("svc-{i:02}")).collect::<Vec<_>>()})),
                "describe-services" => {
                    let names: Vec<&String> = args.iter().skip_while(|a| *a != "--services").skip(1).collect();
                    calls.lock().unwrap().push(names.len());
                    if names.len() > 10 {
                        return Err(AwsError::Api("InvalidParameterException: services can have at most 10 items".into()));
                    }
                    if fail_name.is_some_and(|f| names.iter().any(|n| *n == f)) {
                        return Err(AwsError::Api("ThrottlingException".into()));
                    }
                    Ok(json!({"services": names.iter().map(|n| json!({
                        "serviceName": n, "status": "ACTIVE", "desiredCount": 2, "runningCount": 2,
                        "deployments": [{"status": "PRIMARY", "runningCount": 2, "rolloutState": "COMPLETED"}]
                    })).collect::<Vec<_>>(), "failures": []}))
                }
                "list-tasks" => Ok(json!({"taskArns": []})),
                other => panic!("unexpected op {other}"),
            }
        })
    }

    #[test]
    fn tail_reads_streams_in_batches_of_100_and_skips_lines_already_shown() {
        let calls = Arc::new(Mutex::new(vec![]));
        let seen_calls = calls.clone();
        let aws = Aws::with_runner(move |_, _, op, args| {
            assert_eq!(op, "filter-log-events");
            let names = args.iter().skip_while(|a| *a != "--log-stream-names").skip(1).take_while(|a| !a.starts_with("--")).count();
            seen_calls.lock().unwrap().push(names);
            // Every batch returns the same two events; the second poll adds a third.
            let polls = seen_calls.lock().unwrap().len();
            let mut evs = vec![json!({"eventId": "a", "timestamp": 1000, "logStreamName": "p/c/task1", "message": "one"}),
                               json!({"eventId": "b", "timestamp": 2000, "logStreamName": "p/c/task2", "message": "two"})];
            if polls > 2 {
                evs.push(json!({"eventId": "c", "timestamp": 3000, "logStreamName": "p/c/task1", "message": "three"}));
            }
            Ok(json!({"events": evs}))
        });
        let sel = LogSel { group: "g".into(), streams: (0..150).map(|i| format!("p/c/{i}")).collect(), prefix: None, pattern: String::new(), containers: false };
        let (mut cursor, mut seen) = (0, HashMap::new());
        let (rows, skipped) = tail_poll(&aws, &sel, &mut cursor, &mut seen).unwrap();
        let mut sizes = calls.lock().unwrap().clone();
        sizes.sort();
        assert_eq!(sizes, vec![50, 100]);
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"], "duplicates across batches are dropped");
        assert_eq!(rows[0].cells[1], "task1");
        assert!(!skipped);
        let (rows, _) = tail_poll(&aws, &sel, &mut cursor, &mut seen).unwrap();
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["c"]);
        assert_eq!(cursor, 3000);
    }

    #[test]
    fn services_are_described_in_chunks_of_ten() {
        let calls = Arc::new(Mutex::new(vec![]));
        let t = View::Services { cluster: "c".into() }.fetch(&fake(23, None, calls.clone())).unwrap();
        let mut sizes = calls.lock().unwrap().clone();
        sizes.sort();
        assert_eq!(sizes, vec![3, 10, 10]);
        assert_eq!(t.rows.len(), 23);
        assert!(t.notes.iter().all(|n| n.health != Health::Bad), "{:?}", t.notes);
    }

    #[test]
    fn a_failed_chunk_is_surfaced_not_swallowed() {
        let calls = Arc::new(Mutex::new(vec![]));
        let t = View::Services { cluster: "c".into() }.fetch(&fake(23, Some("svc-15"), calls)).unwrap();
        assert_eq!(t.rows.len(), 13);
        assert!(t.notes.iter().any(|n| n.health == Health::Bad && n.text.contains("INCOMPLETE") && n.text.contains("ThrottlingException")), "{:?}", t.notes);
    }

    #[test]
    fn tasks_merge_running_and_stopped_in_chunks_of_100() {
        let calls = Arc::new(Mutex::new(vec![]));
        let c2 = calls.clone();
        let aws = Aws::with_runner(move |_, _, op, args| match op {
            "list-tasks" => {
                let stopped = args.iter().any(|a| a == "STOPPED");
                let ids: Vec<String> = if stopped { (0..30).map(|i| format!("stopped-{i}")).collect() } else { (0..150).map(|i| format!("run-{i}")).collect() };
                Ok(json!({ "taskArns": ids }))
            }
            "describe-tasks" => {
                let ids: Vec<&String> = args.iter().skip_while(|a| *a != "--tasks").skip(1).collect();
                c2.lock().unwrap().push(ids.len());
                if ids.len() > 100 {
                    return Err(AwsError::Api("InvalidParameterException: tasks can have at most 100 items".into()));
                }
                Ok(json!({ "tasks": ids.iter().map(|id| if id.starts_with("stopped") {
                    json!({"taskArn": id, "lastStatus": "STOPPED", "desiredStatus": "STOPPED", "stopCode": "ServiceSchedulerInitiated",
                           "stoppedReason": "Task failed ELB health checks in (target-group arn:aws:elasticloadbalancing:x:1:targetgroup/tg/abc)",
                           "containers": [{"name": "app", "exitCode": 137, "reason": "OutOfMemoryError: Container killed due to memory usage"}]})
                } else {
                    json!({"taskArn": id, "lastStatus": "RUNNING", "desiredStatus": "RUNNING", "healthStatus": "HEALTHY", "containers": [{"name": "app"}]})
                }).collect::<Vec<_>>(), "failures": [] }))
            }
            other => panic!("unexpected op {other}"),
        });
        let t = View::Tasks { cluster: "c".into(), service: None }.fetch(&aws).unwrap();
        let mut sizes = calls.lock().unwrap().clone();
        sizes.sort();
        assert_eq!(sizes, vec![80, 100]);
        assert_eq!(t.rows.len(), 180);
        let stopped = t.rows.iter().find(|r| r.id == "stopped-0").unwrap();
        assert_eq!(stopped.health, Health::Bad);
        let reason = stopped.cells.last().unwrap();
        assert!(reason.contains("ELB health checks") && reason.contains("exit 137") && reason.contains("OutOfMemoryError"), "{reason}");
        assert!(t.notes.iter().any(|n| n.health == Health::Bad && n.text.contains("30 task(s) stopped with failures")), "{:?}", t.notes);
    }

    #[test]
    fn stopped_tasks_are_classified_by_generation() {
        let sp = split_deployments(&json!({"desiredCount": 4, "deployments": [
            {"id": "ecs-svc/new", "status": "PRIMARY"}, {"id": "ecs-svc/old", "status": "ACTIVE"}]}));
        let drain = json!({"startedBy": "ecs-svc/old", "lastStatus": "STOPPED", "stopCode": "ServiceSchedulerInitiated",
            "stoppedReason": "Scaling activity initiated by (deployment ecs-svc/new)", "containers": [{"exitCode": 143}]});
        let elb = json!({"startedBy": "ecs-svc/new", "lastStatus": "STOPPED", "stopCode": "ServiceSchedulerInitiated",
            "stoppedReason": "Task failed ELB health checks in (target-group arn:aws:elasticloadbalancing:x:1:targetgroup/tg/abc)",
            "containers": [{"exitCode": 0}]});
        let crash = json!({"startedBy": "ecs-svc/new", "lastStatus": "STOPPED", "stopCode": "EssentialContainerExited",
            "stoppedReason": "Essential container in task exited", "containers": [{"exitCode": 1}]});
        assert_eq!(stop_kind(&drain, &sp), "drained (old gen)");
        assert_eq!(task_health(&drain), Health::Dim);
        assert_eq!(stop_kind(&elb, &sp), "failed health check");
        assert_eq!(task_health(&elb), Health::Bad);
        assert_eq!(stop_kind(&crash, &sp), "crashed (non-zero exit)");
        assert_eq!(task_health(&crash), Health::Bad);
    }

    #[test]
    fn scheduler_scale_in_is_not_a_failure() {
        let drained = json!({"lastStatus": "STOPPED", "stopCode": "ServiceSchedulerInitiated",
            "stoppedReason": "Scaling activity initiated by (deployment ecs-svc/123)", "containers": [{"exitCode": 143}]});
        assert_eq!(task_health(&drained), Health::Dim);
    }

    #[test]
    fn sso_expiry_aborts_the_view() {
        let aws = Aws::with_runner(|_, _, _, _| Err(AwsError::SsoExpired("test".into())));
        assert!(matches!(View::Services { cluster: "c".into() }.fetch(&aws), Err(AwsError::SsoExpired(_))));
    }

    #[test]
    fn guard_refuses_mutations() {
        use crate::aws::check_read_only;
        for (svc, op) in [("ecs", "update-service"), ("ecs", "execute-command"), ("ecs", "stop-task"), ("logs", "delete-log-group"),
            ("application-autoscaling", "register-scalable-target"), ("ecr", "get-login-password"), ("secretsmanager", "get-secret-value"),
            ("ec2", "run-instances"), ("sqs", "purge-queue"), ("lambda", "invoke")] {
            assert!(check_read_only(svc, op).is_err(), "{svc} {op} allowed");
        }
        let aws = Aws::with_runner(|_, _, _, _| panic!("runner reached"));
        assert!(aws.call("ecs", "update-service", &[]).is_err());
        for (svc, op) in [("ecs", "describe-services"), ("logs", "filter-log-events"), ("sts", "get-caller-identity")] {
            assert!(check_read_only(svc, op).is_ok());
        }
    }
}
