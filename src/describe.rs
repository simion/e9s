//! `d`: a kubectl-describe style page for the resources people debug most.
//! Aligned `Key: value` lines and sections; anything without a page here falls back to YAML.

use crate::logic::{age_at, arr, image_tag, n, parse_ts, s, short, split_deployments, taskdef};
use crate::views::{Row, View};
use chrono::Local;
use serde_json::Value;

struct Page(Vec<String>);

impl Page {
    fn kv(&mut self, indent: usize, key: &str, value: impl AsRef<str>) {
        let v = value.as_ref();
        if !v.is_empty() {
            self.0.push(format!("{}{:<w$} {v}", " ".repeat(indent), format!("{key}:"), w = 24usize.saturating_sub(indent)));
        }
    }
    fn section(&mut self, title: &str) {
        self.0.push(String::new());
        self.0.push(format!("{title}:"));
    }
    fn line(&mut self, indent: usize, text: impl AsRef<str>) {
        self.0.push(format!("{}{}", " ".repeat(indent), text.as_ref()));
    }
}

/// "2026-10-14 17:55:21 (12m ago)", local time.
fn when(v: &Value, ptr: &str) -> String {
    match v.pointer(ptr).and_then(parse_ts) {
        Some(t) => format!("{} ({} ago)", t.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"), age_at(v, ptr)),
        None => String::new(),
    }
}

/// A plain-text table: header plus rows, columns padded to their widest cell.
fn table(p: &mut Page, indent: usize, header: &[&str], rows: Vec<Vec<String>>) {
    let mut w: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let fmt = |cells: Vec<String>| cells.iter().enumerate().map(|(i, c)| format!("{c:<width$}", width = w[i])).collect::<Vec<_>>().join("  ");
    p.line(indent, fmt(header.iter().map(|h| h.to_string()).collect()).trim_end());
    for r in rows {
        p.line(indent, fmt(r).trim_end());
    }
}

pub fn describe(view: &View, row: &Row) -> Option<Vec<String>> {
    let j = &row.json;
    let mut p = Page(vec![]);
    match view {
        View::Clusters => cluster(&mut p, j),
        View::Services { .. } => service(&mut p, j),
        View::Tasks { .. } | View::Rollout { .. } => task(&mut p, j),
        View::Failures { .. } if j.get("taskArn").is_some() => task(&mut p, j),
        View::Containers { .. } => container(&mut p, j, 0),
        View::TargetGroups { .. } => target_group(&mut p, j),
        View::TaskDefContainers { .. } => container_definition(&mut p, j),
        _ => return None,
    }
    p.line(0, "");
    p.line(0, "(y shows these fields as YAML)");
    Some(p.0)
}

fn cluster(p: &mut Page, c: &Value) {
    p.kv(0, "Name", s(c, "/clusterName"));
    p.kv(0, "Status", s(c, "/status"));
    p.kv(0, "Services", s(c, "/activeServicesCount"));
    p.kv(0, "Tasks", format!("{} running, {} pending", n(c, "/runningTasksCount"), n(c, "/pendingTasksCount")));
    p.kv(0, "Container instances", s(c, "/registeredContainerInstancesCount"));
    let cps: Vec<String> = arr(c, "/capacityProviders").iter().filter_map(|x| x.as_str().map(String::from)).collect();
    p.kv(0, "Capacity providers", cps.join(", "));
    for st in arr(c, "/settings") {
        p.kv(0, &s(st, "/name"), s(st, "/value"));
    }
    p.kv(0, "ARN", s(c, "/clusterArn"));
}

fn service(p: &mut Page, v: &Value) {
    let sp = split_deployments(v);
    p.kv(0, "Name", s(v, "/serviceName"));
    p.kv(0, "Cluster", short(&s(v, "/clusterArn")));
    p.kv(0, "Status", s(v, "/status"));
    p.kv(0, "Created", when(v, "/createdAt"));
    p.kv(0, "Launch type", format!("{} {}", s(v, "/launchType"), s(v, "/platformVersion")).trim());
    p.kv(0, "Task definition", taskdef(&s(v, "/taskDefinition")));
    p.kv(0, "Desired", sp.desired.to_string());
    p.kv(0, "Running", format!("{} (both generations: {} new, {} old)", sp.running_all, sp.new_running(), sp.old_running()));
    p.kv(0, "Pending", sp.pending_all.to_string());
    p.kv(0, "Scheduling", s(v, "/schedulingStrategy"));

    p.section("Rollout settings");
    let dc = &v["deploymentConfiguration"];
    p.kv(2, "Controller", s(v, "/deploymentController/type"));
    let (min, max) = (n(dc, "/minimumHealthyPercent"), n(dc, "/maximumPercent"));
    p.kv(2, "Min healthy", format!("{min}%  (Kubernetes: maxUnavailable {}%)", (100 - min).max(0)));
    p.kv(2, "Max", format!("{max}%  (Kubernetes: maxSurge {}%)", (max - 100).max(0)));
    if dc.get("deploymentCircuitBreaker").is_some() {
        let rollback = dc["deploymentCircuitBreaker"]["rollback"].as_bool() == Some(true);
        p.kv(2, "Circuit breaker", format!(
            "enable={} rollback={rollback}  (stops a rollout whose tasks keep failing{}; Kubernetes has progressDeadlineSeconds but no automatic rollback)",
            s(dc, "/deploymentCircuitBreaker/enable"),
            if rollback { " and redeploys the last good revision" } else { "" },
        ));
    }
    if let Some(g) = v.get("healthCheckGracePeriodSeconds") {
        p.kv(2, "Health check grace", format!("{g}s  (load balancer health checks are ignored this long after a task starts; Kubernetes: startupProbe)"));
    }

    let net = &v["networkConfiguration"]["awsvpcConfiguration"];
    if !net.is_null() {
        p.section("Network");
        p.kv(2, "Subnets", arr(net, "/subnets").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
        p.kv(2, "Security groups", arr(net, "/securityGroups").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
        p.kv(2, "Public IP", s(net, "/assignPublicIp"));
    }

    let lbs = arr(v, "/loadBalancers");
    if !lbs.is_empty() {
        p.section("Load balancers");
        for lb in lbs {
            let arn = s(lb, "/targetGroupArn");
            let tg = arn.split("targetgroup/").nth(1).and_then(|x| x.split('/').next()).unwrap_or(&arn).to_string();
            p.line(2, format!("target group {tg} -> container {}:{}", s(lb, "/containerName"), s(lb, "/containerPort")));
        }
    }

    p.section("Deployments");
    let rows = arr(v, "/deployments")
        .iter()
        .map(|d| {
            vec![
                s(d, "/status"),
                taskdef(&s(d, "/taskDefinition")).to_string(),
                s(d, "/desiredCount"),
                s(d, "/runningCount"),
                s(d, "/pendingCount"),
                s(d, "/failedTasks"),
                s(d, "/rolloutState"),
                age_at(d, "/createdAt"),
            ]
        })
        .collect();
    table(p, 2, &["STATUS", "TASKDEF", "DESIRED", "RUNNING", "PENDING", "FAILED", "ROLLOUT", "AGE"], rows);
    for d in arr(v, "/deployments") {
        let reason = s(d, "/rolloutStateReason");
        if !reason.is_empty() {
            p.line(2, format!("{}: {reason}", s(d, "/status")));
        }
    }

    let events = arr(v, "/events");
    if !events.is_empty() {
        p.section(&format!("Events (newest {} of {}; e shows all)", events.len().min(20), events.len()));
        for e in events.iter().take(20) {
            p.line(2, format!("{:>6}  {}", age_at(e, "/createdAt"), crate::views::short_event(&s(e, "/message"))));
        }
    }
    p.line(0, "");
    p.kv(0, "ARN", s(v, "/serviceArn"));
}

fn task(p: &mut Page, t: &Value) {
    let arn = s(t, "/taskArn");
    p.kv(0, "Task", short(&arn));
    p.kv(0, "Cluster", short(&s(t, "/clusterArn")));
    p.kv(0, "Group", s(t, "/group"));
    p.kv(0, "Started by", s(t, "/startedBy"));
    p.kv(0, "Task definition", taskdef(&s(t, "/taskDefinitionArn")));
    p.kv(0, "Status", format!("{} (desired {})", s(t, "/lastStatus"), s(t, "/desiredStatus")));
    p.kv(0, "Health", s(t, "/healthStatus"));
    if !s(t, "/stoppedReason").is_empty() || !s(t, "/stopCode").is_empty() {
        p.kv(0, "Stop code", s(t, "/stopCode"));
        p.kv(0, "Stopped reason", s(t, "/stoppedReason"));
    }
    p.kv(0, "Launch", format!("{} {} {}", s(t, "/launchType"), s(t, "/platformVersion"), s(t, "/capacityProviderName")).trim());
    p.kv(0, "Zone", s(t, "/availabilityZone"));
    p.kv(0, "CPU / memory", format!("{} / {} (reserved)", s(t, "/cpu"), s(t, "/memory")));
    let ip = arr(t, "/containers").iter().flat_map(|c| arr(c, "/networkInterfaces")).map(|ni| s(ni, "/privateIpv4Address")).find(|x| !x.is_empty());
    p.kv(0, "Private IP", ip.unwrap_or_default());

    // In time order: a crash stops execution before ECS starts stopping the task; a scheduler stop is the other way round.
    p.section("Timeline");
    let mut steps = [
        ("Created", "/createdAt"),
        ("Image pull started", "/pullStartedAt"),
        ("Image pull finished", "/pullStoppedAt"),
        ("Started", "/startedAt"),
        ("Stopping", "/stoppingAt"),
        ("Stopped", "/stoppedAt"),
        ("Execution stopped", "/executionStoppedAt"),
    ];
    steps.sort_by_key(|(_, ptr)| t.pointer(ptr).and_then(parse_ts));
    for (label, ptr) in steps {
        p.kv(2, label, when(t, ptr));
    }

    p.section("Containers");
    for c in arr(t, "/containers") {
        container(p, c, 2);
    }
    p.line(0, "");
    p.kv(0, "ARN", arn);
}

fn container(p: &mut Page, c: &Value, indent: usize) {
    p.line(indent, s(c, "/name"));
    let i = indent + 2;
    p.kv(i, "Image", s(c, "/image"));
    p.kv(i, "Tag", image_tag(&s(c, "/image")));
    p.kv(i, "Digest", s(c, "/imageDigest"));
    p.kv(i, "State", s(c, "/lastStatus"));
    p.kv(i, "Exit code", s(c, "/exitCode"));
    p.kv(i, "Reason", s(c, "/reason"));
    p.kv(i, "Health", s(c, "/healthStatus"));
    // Containers without their own limits share the task's; ECS reports cpu "0" for them.
    let (cpu, mem) = (s(c, "/cpu"), s(c, "/memory"));
    if (!cpu.is_empty() && cpu != "0") || !mem.is_empty() {
        p.kv(i, "CPU / memory", format!("{} / {}", if cpu.is_empty() { "-" } else { &cpu }, if mem.is_empty() { "-" } else { &mem }));
    }
    p.kv(i, "Runtime id", s(c, "/runtimeId"));
}

/// One container of a task definition: what it runs, its probes, and its configuration names.
fn container_definition(p: &mut Page, c: &Value) {
    p.kv(0, "Name", s(c, "/name"));
    p.kv(0, "Image", s(c, "/image"));
    p.kv(0, "Essential", format!("{}  (if an essential container stops, ECS stops the whole task)", s(c, "/essential")));
    // Without its own limits a container shares the task's; ECS reports cpu 0 then.
    let (cpu, mem) = (s(c, "/cpu"), s(c, "/memory"));
    if (!cpu.is_empty() && cpu != "0") || !mem.is_empty() {
        p.kv(0, "CPU / memory", format!("{} / {}", if cpu.is_empty() { "-" } else { &cpu }, if mem.is_empty() { "-" } else { &mem }));
    } else {
        p.kv(0, "CPU / memory", "shares the task's (no per-container limit)");
    }
    p.kv(0, "Memory soft limit", s(c, "/memoryReservation"));
    p.kv(0, "Command", arr(c, "/command").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "));
    p.kv(0, "Entry point", arr(c, "/entryPoint").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "));
    p.kv(0, "Stop timeout", c.get("stopTimeout").map(|t| format!("{t}s  (Kubernetes: terminationGracePeriodSeconds)")).unwrap_or_default());
    let ports: Vec<String> = arr(c, "/portMappings").iter().map(|m| format!("{}/{}", s(m, "/containerPort"), s(m, "/protocol"))).collect();
    p.kv(0, "Ports", ports.join(", "));
    let hc = &c["healthCheck"];
    if !hc.is_null() {
        p.section("Health check (Kubernetes: liveness probe)");
        p.kv(2, "Command", arr(hc, "/command").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "));
        p.kv(2, "Interval / timeout", format!("{}s / {}s", s(hc, "/interval"), s(hc, "/timeout")));
        p.kv(2, "Retries", s(hc, "/retries"));
        p.kv(2, "Start period", hc.get("startPeriod").map(|x| format!("{x}s")).unwrap_or_default());
    }
    let deps: Vec<String> = arr(c, "/dependsOn").iter().map(|d| format!("{} ({})", s(d, "/containerName"), s(d, "/condition"))).collect();
    p.kv(0, "Depends on", deps.join(", "));
    p.section("Logging");
    p.kv(2, "Driver", s(c, "/logConfiguration/logDriver"));
    p.kv(2, "Group", s(c, "/logConfiguration/options/awslogs-group"));
    p.kv(2, "Stream prefix", s(c, "/logConfiguration/options/awslogs-stream-prefix"));
    let names = |k: &str| arr(c, k).iter().map(|e| s(e, "/name")).collect::<Vec<_>>();
    let env = names("/environment");
    p.section(&format!("Environment variables ({}; names only, values are never shown)", env.len()));
    for e in env {
        p.line(2, e);
    }
    let secrets = names("/secrets");
    if !secrets.is_empty() {
        p.section(&format!("Secrets ({}; injected from Secrets Manager or SSM, names only)", secrets.len()));
        for e in secrets {
            p.line(2, e);
        }
    }
}

fn target_group(p: &mut Page, tg: &Value) {
    p.kv(0, "Name", s(tg, "/TargetGroupName"));
    p.kv(0, "Protocol / port", format!("{}:{}", s(tg, "/Protocol"), s(tg, "/Port")));
    p.kv(0, "Target type", s(tg, "/TargetType"));
    p.kv(0, "VPC", s(tg, "/VpcId"));
    p.kv(0, "Load balancers", arr(tg, "/LoadBalancerArns").iter().filter_map(Value::as_str).map(|a| a.split('/').nth(2).unwrap_or(a)).collect::<Vec<_>>().join(", "));

    p.section("Health check (Kubernetes: readiness probe)");
    p.kv(2, "Target", format!("{} {}{}", s(tg, "/HealthCheckProtocol"), s(tg, "/HealthCheckPort"), s(tg, "/HealthCheckPath")));
    p.kv(2, "Interval / timeout", format!("{}s / {}s", s(tg, "/HealthCheckIntervalSeconds"), s(tg, "/HealthCheckTimeoutSeconds")));
    p.kv(2, "Healthy after", format!("{} passes", s(tg, "/HealthyThresholdCount")));
    p.kv(2, "Unhealthy after", format!("{} failures", s(tg, "/UnhealthyThresholdCount")));
    let every = n(tg, "/HealthCheckIntervalSeconds");
    p.kv(2, "Time to healthy", format!("about {}  (a new target gets traffic after this)", crate::logic::fmt_dur(n(tg, "/HealthyThresholdCount") * every)));
    p.kv(2, "Time to unhealthy", format!("about {}  (a broken target keeps getting traffic this long)", crate::logic::fmt_dur(n(tg, "/UnhealthyThresholdCount") * every)));
    p.kv(2, "Success codes", s(tg, "/Matcher/HttpCode"));

    if let Some(attrs) = tg["Attributes"].as_object().filter(|a| !a.is_empty()) {
        p.section("Attributes");
        let mut keys: Vec<&String> = attrs.keys().collect();
        keys.sort();
        for k in keys {
            p.kv(2, k, attrs[k].as_str().unwrap_or_default());
        }
    }
    p.line(0, "");
    p.kv(0, "ARN", s(tg, "/TargetGroupArn"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::Health;
    use serde_json::json;

    #[test]
    fn task_page_leads_with_the_stop_reason_and_lists_containers() {
        let row = Row {
            id: "t".into(),
            cells: vec![],
            health: Health::Bad,
            json: json!({
                "taskArn": "arn:aws:ecs:us-west-1:1:task/prod/abc123", "lastStatus": "STOPPED", "desiredStatus": "STOPPED",
                "stopCode": "EssentialContainerExited", "stoppedReason": "Essential container in task exited",
                "createdAt": "2026-10-14T10:00:00Z", "stoppedAt": "2026-10-14T10:05:00Z",
                "stoppingAt": "2026-10-14T10:04:30Z", "executionStoppedAt": "2026-10-14T10:04:00Z",
                "containers": [{"name": "app", "image": "repo/app:v42", "lastStatus": "STOPPED", "exitCode": 137, "reason": "OutOfMemoryError"}]
            }),
        };
        let page = describe(&View::Tasks { cluster: "prod".into(), service: None }, &row).unwrap().join("\n");
        assert!(page.contains("Stopped reason:") && page.contains("Essential container in task exited"), "{page}");
        assert!(page.contains("Exit code:") && page.contains("137") && page.contains("OutOfMemoryError"), "{page}");
        assert!(page.contains("Tag:") && page.contains("v42"), "{page}");
        assert!(!page.contains("Image pull started"), "empty fields are skipped: {page}");
        let at = |k: &str| page.find(k).unwrap();
        assert!(at("Execution stopped") < at("Stopping") && at("Stopping") < at("Stopped:"), "timeline in time order: {page}");
    }
}

/// RDS instance or cluster: what it is now, what is changing, and its events of the last day (newest last).
/// RDS reports no percentage for a modification; its steps arrive as events, so those are the progress.
pub fn rds(v: &Value, events: &[Value]) -> Vec<String> {
    let mut p = Page(vec![]);
    let cluster = v.get("DBClusterIdentifier").is_some() && v.get("DBInstanceIdentifier").is_none();
    let status = if cluster { s(v, "/Status") } else { s(v, "/DBInstanceStatus") };
    p.kv(0, "Name", if cluster { s(v, "/DBClusterIdentifier") } else { s(v, "/DBInstanceIdentifier") });
    p.kv(0, "Kind", if cluster { "cluster" } else { "instance" });
    p.kv(0, "Status", match rds_status(&status) {
        "" => status.clone(),
        m => format!("{status}  ({m})"),
    });
    p.kv(0, "Engine", format!("{} {}", s(v, "/Engine"), s(v, "/EngineVersion")).trim());
    p.kv(0, "Class", if cluster { s(v, "/DBClusterInstanceClass") } else { s(v, "/DBInstanceClass") });
    if !cluster {
        let mut st = format!("{} GiB {}", s(v, "/AllocatedStorage"), s(v, "/StorageType"));
        if v.get("Iops").is_some() {
            st += &format!(", {} IOPS", s(v, "/Iops"));
        }
        if v.get("StorageThroughput").is_some() {
            st += &format!(", {} MiB/s", s(v, "/StorageThroughput"));
        }
        if v.get("MaxAllocatedStorage").is_some() {
            st += &format!(", autoscales to {} GiB", s(v, "/MaxAllocatedStorage"));
        }
        p.kv(0, "Storage", st);
        p.kv(0, "Availability zone", s(v, "/AvailabilityZone"));
    }
    p.kv(0, "Multi-AZ", s(v, "/MultiAZ"));
    p.kv(0, "Endpoint", if cluster {
        s(v, "/Endpoint")
    } else {
        format!("{}:{}", s(v, "/Endpoint/Address"), s(v, "/Endpoint/Port")).trim_matches(':').to_string()
    });
    p.kv(0, "Reader endpoint", s(v, "/ReaderEndpoint"));
    p.kv(0, "Created", when(v, if cluster { "/ClusterCreateTime" } else { "/InstanceCreateTime" }));

    let pending = crate::views::pending(v);
    p.section("Pending changes");
    if pending.is_empty() {
        p.line(2, "none");
    } else {
        for c in &pending {
            p.line(2, c);
        }
        let mw = s(v, "/PreferredMaintenanceWindow");
        p.line(2, if status == "available" {
            format!("(queued: applied in the maintenance window {mw} UTC, or on the next modify with apply-immediately)")
        } else {
            "(being applied now; each leaves this list when done. A Single-AZ class change restarts the database)".into()
        });
    }

    // The newest "Applying ..." event with no "Finished ..." after it: how long the change has been running.
    let started = events.iter().rposition(|e| s(e, "/Message").starts_with("Applying"));
    let finished = started.is_some_and(|i| events[i + 1..].iter().any(|e| s(e, "/Message").starts_with("Finished applying")));
    if let (Some(i), false) = (started, finished) {
        p.kv(0, "Applying since", when(&events[i], "/Date"));
    }

    p.section("Maintenance and backups");
    p.kv(2, "Maintenance window", s(v, "/PreferredMaintenanceWindow"));
    p.kv(2, "Backup window", s(v, "/PreferredBackupWindow"));
    p.kv(2, "Backup retention", if v.get("BackupRetentionPeriod").is_some() { format!("{} days", s(v, "/BackupRetentionPeriod")) } else { String::new() });
    p.kv(2, "Latest restorable", when(v, "/LatestRestorableTime"));
    p.kv(2, "Auto minor upgrade", s(v, "/AutoMinorVersionUpgrade"));
    p.kv(2, "CA certificate", s(v, "/CACertificateIdentifier"));

    p.section("Security");
    p.kv(2, "Deletion protection", s(v, "/DeletionProtection"));
    p.kv(2, "Publicly accessible", s(v, "/PubliclyAccessible"));
    p.kv(2, "Storage encrypted", s(v, "/StorageEncrypted"));
    p.kv(2, "Performance Insights", s(v, "/PerformanceInsightsEnabled"));

    p.section("Events, last 24h (refreshes with the page)");
    if events.is_empty() {
        p.line(2, "none");
    }
    for e in events {
        let at = e.pointer("/Date").and_then(parse_ts).map(|t| t.with_timezone(&Local).format("%m-%d %H:%M:%S").to_string()).unwrap_or_default();
        p.line(2, format!("{at}  {:>8}  {}", age_at(e, "/Date"), s(e, "/Message")));
    }
    p.kv(0, "ARN", if cluster { s(v, "/DBClusterArn") } else { s(v, "/DBInstanceArn") });
    p.line(0, "");
    p.line(0, "(y shows these fields as YAML)");
    p.0
}

/// What an RDS status means while it lasts, for the ones that are not plain.
fn rds_status(s: &str) -> &'static str {
    match s {
        "modifying" => "a change is being applied; see Pending changes and Events",
        "storage-optimization" => "storage was resized and is being optimized in the background; usable, no further storage change for up to 6h",
        "rebooting" => "restarting; connections drop",
        "upgrading" => "engine version upgrade; unavailable during it",
        "backing-up" => "snapshot in progress; usable",
        "maintenance" => "AWS maintenance is being applied",
        "configuring-enhanced-monitoring" | "configuring-log-exports" | "configuring-iam-database-auth" => "a setting is being applied; usable",
        _ => "",
    }
}

/// Lambda function from get-function-configuration: what it runs, and whether its last change has settled.
pub fn lambda(c: &Value) -> Vec<String> {
    let mut p = Page(vec![]);
    let (_, status) = crate::views::lambda_status(c);
    p.kv(0, "Name", s(c, "/FunctionName"));
    p.kv(0, "Status", match status.as_str() {
        "updating" => "updating  (a code or configuration change is being applied; invocations still run the previous version)".into(),
        "pending" => "pending  (being created; it cannot be invoked yet)".into(),
        "inactive" => "inactive  (idle for weeks; the first invocation wakes it and may fail while it does)".into(),
        _ => status.clone(),
    });
    p.kv(0, "State", format!("{} {}", s(c, "/State"), s(c, "/StateReasonCode")).trim());
    p.kv(0, "State reason", s(c, "/StateReason"));
    p.kv(0, "Last update", format!("{} {}", s(c, "/LastUpdateStatus"), s(c, "/LastUpdateStatusReasonCode")).trim());
    p.kv(0, "Last update reason", s(c, "/LastUpdateStatusReason"));
    p.kv(0, "Last modified", when(c, "/LastModified"));
    p.kv(0, "Description", s(c, "/Description"));

    p.section("Code");
    p.kv(2, "Package", s(c, "/PackageType"));
    p.kv(2, "Runtime", s(c, "/Runtime"));
    p.kv(2, "Handler", s(c, "/Handler"));
    p.kv(2, "Architectures", arr(c, "/Architectures").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
    p.kv(2, "Size", format!("{:.1} MB", n(c, "/CodeSize") as f64 / 1e6));
    p.kv(2, "SHA-256", s(c, "/CodeSha256"));
    p.kv(2, "Version", s(c, "/Version"));
    p.kv(2, "Revision", s(c, "/RevisionId"));
    for l in arr(c, "/Layers") {
        p.kv(2, "Layer", short(&s(l, "/Arn")));
    }

    p.section("Limits");
    p.kv(2, "Memory", format!("{} MB", s(c, "/MemorySize")));
    p.kv(2, "Timeout", format!("{}s", s(c, "/Timeout")));
    p.kv(2, "Ephemeral storage", if c.pointer("/EphemeralStorage/Size").is_some() { format!("{} MB", s(c, "/EphemeralStorage/Size")) } else { String::new() });

    if c.get("VpcConfig").is_some() {
        p.section("Network");
        p.kv(2, "VPC", s(c, "/VpcConfig/VpcId"));
        p.kv(2, "Subnets", arr(c, "/VpcConfig/SubnetIds").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
        p.kv(2, "Security groups", arr(c, "/VpcConfig/SecurityGroupIds").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
    }
    p.kv(0, "Role", s(c, "/Role"));
    p.kv(0, "Log group", s(c, "/LoggingConfig/LogGroup"));
    p.kv(0, "ARN", s(c, "/FunctionArn"));
    p.line(0, "");
    p.line(0, "(refreshes with the page; y shows these fields as YAML)");
    p.0
}
