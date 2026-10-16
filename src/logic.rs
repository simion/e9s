//! Pure helpers: no I/O, all covered by tests at the bottom.

use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Health {
    Ok,
    Warn,
    Bad,
    Dim,
    None,
}

pub fn s(v: &Value, ptr: &str) -> String {
    match v.pointer(ptr) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(x) => x.to_string(),
    }
}

pub fn n(v: &Value, ptr: &str) -> i64 {
    v.pointer(ptr).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))).unwrap_or(0)
}

pub fn arr<'a>(v: &'a Value, ptr: &str) -> &'a [Value] {
    v.pointer(ptr).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub fn strs(v: &Value, ptr: &str) -> Vec<String> {
    arr(v, ptr).iter().filter_map(|x| x.as_str().map(String::from)).collect()
}

/// Last path segment of an ARN-ish string ("…/name" or "…:name").
pub fn short(arn: &str) -> &str {
    arn.rsplit(['/', ':']).next().unwrap_or(arn)
}

/// "family:rev" from a task definition ARN.
pub fn taskdef(arn: &str) -> &str {
    arn.rsplit('/').next().unwrap_or(arn)
}

pub fn image_tag(image: &str) -> &str {
    if let Some((_, d)) = image.split_once('@') {
        return &d[..d.len().min(19)];
    }
    match image.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') && !repo.is_empty() => tag,
        _ => "latest",
    }
}

// ---------- ECS deployments ----------

#[derive(Clone, Debug, PartialEq)]
pub struct Deployment {
    pub id: String,
    pub desired: i64,
    pub running: i64,
    pub pending: i64,
    pub rollout: String,
    pub created: String,
    pub taskdef: String,
}

/// PRIMARY is the new generation, ACTIVE the old ones being drained.
/// `desired` is the service's desiredCount: the only valid denominator.
/// The service-level runningCount is the sum of both generations, so it is never rollout progress.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    pub desired: i64,
    pub running_all: i64,
    pub pending_all: i64,
    pub primary: Option<Deployment>,
    pub old: Vec<Deployment>,
}

impl Split {
    pub fn old_running(&self) -> i64 {
        self.old.iter().map(|d| d.running).sum()
    }
    pub fn new_running(&self) -> i64 {
        self.primary.as_ref().map_or(0, |d| d.running)
    }
    pub fn health(&self, status: &str) -> Health {
        let rollout = self.primary.as_ref().map_or("", |d| d.rollout.as_str());
        if rollout == "FAILED" || (self.desired > 0 && self.running_all == 0) {
            Health::Bad
        } else if !self.old.is_empty() || rollout == "IN_PROGRESS" || status != "ACTIVE" || self.new_running() != self.desired {
            Health::Warn
        } else {
            Health::Ok
        }
    }
}

/// Rollout progress: new-generation running over the SERVICE desiredCount.
/// Not the PRIMARY deployment's desiredCount (ECS ramps it during the roll),
/// and never the service runningCount (both generations summed).
pub fn progress(sp: &Split) -> (i64, i64, f64) {
    let (num, den) = (sp.new_running(), sp.desired);
    (num, den, if den > 0 { num as f64 * 100.0 / den as f64 } else { 0.0 })
}

pub fn split_deployments(svc: &Value) -> Split {
    let dep = |d: &Value| Deployment {
        id: s(d, "/id"),
        desired: n(d, "/desiredCount"),
        running: n(d, "/runningCount"),
        pending: n(d, "/pendingCount"),
        rollout: s(d, "/rolloutState"),
        created: s(d, "/createdAt"),
        taskdef: taskdef(&s(d, "/taskDefinition")).to_string(),
    };
    let deps = arr(svc, "/deployments");
    Split {
        desired: n(svc, "/desiredCount"),
        running_all: n(svc, "/runningCount"),
        pending_all: n(svc, "/pendingCount"),
        primary: deps.iter().find(|d| s(d, "/status") == "PRIMARY").map(dep),
        old: deps.iter().filter(|d| s(d, "/status") == "ACTIVE").map(dep).collect(),
    }
}

// ---------- secrets ----------

pub const REDACTED: &str = "<redacted>";

/// Keys whose string value is a secret wherever they appear (CloudTrail request parameters, mostly).
fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["password", "secretstring", "secretbinary", "secretaccesskey", "sessiontoken", "privatekey"].iter().any(|s| k.contains(s))
}

/// Strip env var and secret values anywhere in a document. Names stay.
pub fn redact(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, child) in m.iter_mut() {
                match (k.as_str(), &mut *child) {
                    ("environment" | "secrets" | "environmentFiles", Value::Array(items)) => {
                        for it in items {
                            for f in ["value", "valueFrom"] {
                                if let Some(x) = it.get_mut(f) {
                                    *x = Value::String(REDACTED.into());
                                }
                            }
                        }
                    }
                    // CloudTrail request parameters spell it lowercase.
                    ("Environment" | "environment", Value::Object(env)) => {
                        for (k, vars) in env.iter_mut() {
                            if let ("Variables" | "variables", Value::Object(vars)) = (k.as_str(), vars) {
                                for x in vars.values_mut() {
                                    *x = Value::String(REDACTED.into());
                                }
                            }
                        }
                    }
                    (k, Value::String(x)) if is_secret_key(k) => *x = REDACTED.into(),
                    _ => redact(child),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

// ---------- time ----------

pub fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(num) => {
            let f = num.as_f64()?;
            let ms = if f > 1e11 { f } else { f * 1000.0 };
            DateTime::from_timestamp_millis(ms as i64)
        }
        Value::String(x) => DateTime::parse_from_rfc3339(x)
            .or_else(|_| DateTime::parse_from_str(x, "%Y-%m-%dT%H:%M:%S%.f%z"))
            .ok()
            .map(|d| d.with_timezone(&Utc)),
        _ => None,
    }
}

pub fn fmt_dur(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match () {
        _ if d > 0 => format!("{d}d{h}h"),
        _ if h > 0 => format!("{h}h{m}m"),
        _ if m > 0 => format!("{m}m"),
        _ => format!("{secs}s"),
    }
}

pub fn age(v: &Value) -> String {
    parse_ts(v).map(|t| fmt_dur((Utc::now() - t).num_seconds())).unwrap_or_default()
}

pub fn age_at(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).map(age).unwrap_or_default()
}

// ---------- sorting ----------

/// Durations ("3d4h") sort by seconds, numbers ("12/20", "4.5") by value, everything else as text.
#[derive(PartialEq, PartialOrd)]
pub enum SortKey {
    Num(f64),
    Text(String),
}

pub fn sort_key(cell: &str) -> SortKey {
    if let Some(secs) = parse_dur(cell) {
        return SortKey::Num(secs as f64);
    }
    let lead: String = cell.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
    match lead.parse::<f64>() {
        Ok(f) if !lead.is_empty() => SortKey::Num(f),
        _ => SortKey::Text(cell.to_lowercase()),
    }
}

fn parse_dur(cell: &str) -> Option<i64> {
    if cell.is_empty() || !cell.ends_with(['s', 'm', 'h', 'd']) {
        return None;
    }
    let (mut total, mut num) = (0i64, String::new());
    for c in cell.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let mult = match c { 's' => 1, 'm' => 60, 'h' => 3600, 'd' => 86400, _ => return None };
            total += num.parse::<i64>().ok()? * mult;
            num.clear();
        }
    }
    num.is_empty().then_some(total)
}

// ---------- health by status word ----------

pub fn health_of(status: &str) -> Health {
    match status.to_lowercase().as_str() {
        "running" | "healthy" | "active" | "available" | "successful" | "completed" | "steady" | "in-use" => Health::Ok,
        "failed" | "unhealthy" | "stopped-error" | "incompatible-parameters" | "incompatible-network"
        | "inaccessible-encryption-credentials" | "storage-full" | "impaired" => Health::Bad,
        "stopped" | "terminated" | "unused" | "inactive" | "" => Health::Dim,
        _ => Health::Warn,
    }
}

// ---------- describe rendering ----------

/// YAML-ish view of a JSON document for the describe pane.
pub fn to_yaml(v: &Value) -> Vec<String> {
    fn scalar(v: &Value) -> String {
        match v {
            Value::String(x) if x.is_empty() || x.contains(": ") || x.starts_with(['-', '{', '[']) => format!("{x:?}"),
            Value::String(x) => x.clone(),
            Value::Array(a) if a.is_empty() => "[]".into(),
            Value::Object(o) if o.is_empty() => "{}".into(),
            other => other.to_string(),
        }
    }
    fn simple(v: &Value) -> bool {
        match v {
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => true,
        }
    }
    fn walk(v: &Value, ind: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(ind);
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if simple(x) {
                        out.push(format!("{pad}{k}: {}", scalar(x)));
                    } else {
                        out.push(format!("{pad}{k}:"));
                        walk(x, ind + 1, out);
                    }
                }
            }
            Value::Array(a) => {
                for x in a {
                    if simple(x) {
                        out.push(format!("{pad}- {}", scalar(x)));
                    } else {
                        let mut sub = vec![];
                        walk(x, ind + 1, &mut sub);
                        if let Some(first) = sub.first_mut() {
                            *first = format!("{pad}- {}", first.trim_start());
                        }
                        out.extend(sub);
                    }
                }
            }
            x => out.push(format!("{pad}{}", scalar(x))),
        }
    }
    let mut out = vec![];
    walk(v, 0, &mut out);
    out
}

pub fn sparkline(vals: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let (lo, hi) = vals.iter().fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    vals.iter()
        .map(|&v| if hi > lo { BARS[(((v - lo) / (hi - lo)) * 7.0).round() as usize] } else { BARS[0] })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_counts_generations_against_service_desired() {
        // Mid-rollout at 110%: service runningCount 22 is both generations summed.
        let svc = json!({
            "status": "ACTIVE", "desiredCount": 20, "runningCount": 22, "pendingCount": 1,
            "deployments": [
                {"id": "ecs-svc/new", "status": "PRIMARY", "desiredCount": 20, "runningCount": 9, "pendingCount": 1,
                 "rolloutState": "IN_PROGRESS", "taskDefinition": "arn:aws:ecs:us-west-1:1:task-definition/app:42"},
                {"id": "ecs-svc/old", "status": "ACTIVE", "desiredCount": 13, "runningCount": 13, "pendingCount": 0,
                 "rolloutState": "COMPLETED", "taskDefinition": "arn:aws:ecs:us-west-1:1:task-definition/app:41"}
            ]
        });
        let sp = split_deployments(&svc);
        assert_eq!(sp.desired, 20);
        assert_eq!(sp.new_running(), 9);
        assert_eq!(sp.old_running(), 13);
        assert_eq!(sp.running_all, 22);
        assert_eq!(sp.primary.as_ref().unwrap().taskdef, "app:42");
        assert_eq!(sp.old[0].id, "ecs-svc/old");
        assert_eq!(sp.health("ACTIVE"), Health::Warn);
    }

    #[test]
    fn progress_uses_service_desired_not_primary_ramp() {
        let svc = json!({"desiredCount": 128, "runningCount": 140, "deployments": [
            {"status": "PRIMARY", "desiredCount": 25, "runningCount": 12},
            {"status": "ACTIVE", "desiredCount": 116, "runningCount": 128}]});
        let (num, den, pct) = progress(&split_deployments(&svc));
        assert_eq!((num, den), (12, 128));
        assert!((pct - 9.375).abs() < 1e-9, "{pct}");
    }

    #[test]
    fn split_health() {
        let one = |rollout: &str, running: i64| json!({"status": "ACTIVE", "desiredCount": 3, "runningCount": running,
            "deployments": [{"status": "PRIMARY", "runningCount": running, "rolloutState": rollout}]});
        assert_eq!(split_deployments(&one("COMPLETED", 3)).health("ACTIVE"), Health::Ok);
        assert_eq!(split_deployments(&one("FAILED", 3)).health("ACTIVE"), Health::Bad);
        assert_eq!(split_deployments(&one("COMPLETED", 0)).health("ACTIVE"), Health::Bad);
        assert_eq!(split_deployments(&one("COMPLETED", 2)).health("ACTIVE"), Health::Warn);
    }

    #[test]
    fn redacts_env_values_and_secrets_everywhere() {
        let mut td = json!({
            "containerDefinitions": [{"name": "app",
                "environment": [{"name": "DB_URL", "value": "postgres://u:p@h"}],
                "secrets": [{"name": "API_KEY", "valueFrom": "arn:aws:secretsmanager:x"}]}],
            "overrides": {"containerOverrides": [{"environment": [{"name": "X", "value": "y"}]}]},
            "Environment": {"Variables": {"TOKEN": "abc"}},
            // CloudTrail request parameters.
            "requestParameters": {"environment": {"variables": {"K": "lower"}}, "masterUserPassword": "hunter2", "secretString": "s3cr3t"}
        });
        redact(&mut td);
        let text = td.to_string();
        for leaked in ["postgres://", "secretsmanager", "\"y\"", "abc", "lower", "hunter2", "s3cr3t"] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
        assert_eq!(td.pointer("/containerDefinitions/0/environment/0/name").unwrap(), "DB_URL");
    }

    #[test]
    fn sort_keys() {
        assert!(sort_key("3d4h") > sort_key("23h59m"));
        assert!(sort_key("10") > sort_key("9"));
        assert!(sort_key("12/20") > sort_key("3/20"));
        assert!(sort_key("beta") > sort_key("Alpha"));
    }

    #[test]
    fn short_names_and_tags() {
        assert_eq!(short("arn:aws:ecs:us-west-1:1:service/cl/api"), "api");
        assert_eq!(taskdef("arn:aws:ecs:us-west-1:1:task-definition/app:42"), "app:42");
        assert_eq!(image_tag("1.dkr.ecr.us-west-1.amazonaws.com/app:2026.10.01-abc"), "2026.10.01-abc");
        assert_eq!(image_tag("localhost:5000/app"), "latest");
    }
}
