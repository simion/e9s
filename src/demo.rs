//! `E9S_DEMO=1`: a made-up account answers every read, for screenshots and recordings that show
//! nothing real. The runner sits where `sdk::run` does, behind the same read-only guard.
//! One cluster (`shop-prod`) mid-rollout, one service crash-looping, live logs that keep coming.

use crate::aws::{AwsError, Ctx};
use crate::logic::REDACTED;
use chrono::{Duration, SecondsFormat, Utc};
use serde_json::{json, Value};

const ACCT: &str = "123456789012";
const CLUSTER: &str = "shop-prod";

struct Svc {
    name: &'static str,
    desired: usize,
    cpu: u32,
    mem: u32,
    port: Option<u16>,
    rev: u32,
}

const SVCS: &[Svc] = &[
    Svc { name: "web", desired: 6, cpu: 512, mem: 1024, port: Some(3000), rev: 87 },
    Svc { name: "api", desired: 8, cpu: 1024, mem: 2048, port: Some(8080), rev: 42 },
    Svc { name: "checkout", desired: 4, cpu: 512, mem: 1024, port: Some(8080), rev: 31 },
    Svc { name: "payments-worker", desired: 3, cpu: 512, mem: 1024, port: None, rev: 19 },
    Svc { name: "search-indexer", desired: 2, cpu: 1024, mem: 4096, port: None, rev: 12 },
    Svc { name: "image-resizer", desired: 2, cpu: 256, mem: 512, port: None, rev: 9 },
    Svc { name: "email-sender", desired: 2, cpu: 256, mem: 512, port: None, rev: 23 },
    Svc { name: "cron-reports", desired: 1, cpu: 256, mem: 512, port: None, rev: 6 },
];

/// `api` is rolling out: 3 of 8 tasks run the new revision. `image-resizer` runs out of memory.
const ROLLING: &str = "api";
const ROLL_NEW: usize = 3;
const CRASHING: &str = "image-resizer";

pub fn run(ctx: &Ctx, service: &str, op: &str, args: &[String]) -> Result<Value, AwsError> {
    let a = Args(args);
    let r = ctx.region.as_str();
    std::thread::sleep(std::time::Duration::from_millis(120));
    Ok(match (service, op) {
        ("sts", "get-caller-identity") => json!({
            "Account": ACCT, "UserId": "AROADEMO:demo",
            "Arn": format!("arn:aws:sts::{ACCT}:assumed-role/AWSReservedSSO_ReadOnlyAccess_0a1b2c3d4e5f6789/demo"),
        }),
        ("ec2", "describe-regions") => {
            let names = ["us-east-1", "us-east-2", "us-west-1", "us-west-2", "eu-west-1", "eu-central-1", "ap-southeast-1", "ap-northeast-1"];
            json!({"Regions": names.iter().map(|n| json!({"RegionName": n})).collect::<Vec<_>>()})
        }
        ("ec2", "describe-instances") => {
            let i = |id: &str, ty: &str, name: &str, ip: &str, days: i64| json!({"Instances": [{
                "InstanceId": id, "InstanceType": ty, "State": {"Name": "running"}, "Placement": {"AvailabilityZone": format!("{r}a")},
                "PrivateIpAddress": ip, "LaunchTime": ago(Duration::days(days)), "ImageId": "ami-0abcdef1234567890",
                "VpcId": "vpc-0demo", "SubnetId": "subnet-0demo1", "Tags": [{"Key": "Name", "Value": name}],
            }]});
            json!({"Reservations": [
                i("i-0a1b2c3d4e5f60001", "t3.micro", "bastion", "10.0.1.10", 120),
                i("i-0a1b2c3d4e5f60002", "c6i.xlarge", "ci-runner-1", "10.0.2.21", 9),
                i("i-0a1b2c3d4e5f60003", "c6i.xlarge", "ci-runner-2", "10.0.2.22", 9),
                i("i-0a1b2c3d4e5f60004", "r6g.large", "vpn-gateway", "10.0.1.11", 300),
            ]})
        }

        ("ecs", "list-clusters") => {
            let names: &[&str] = match r { "us-east-1" => &[CLUSTER, "shop-staging", "data-jobs"], "eu-west-1" => &["shop-eu"], _ => &[] };
            json!({"clusterArns": names.iter().map(|c| format!("arn:aws:ecs:{r}:{ACCT}:cluster/{c}")).collect::<Vec<_>>()})
        }
        ("ecs", "describe-clusters") => {
            let cs: Vec<Value> = a.all("--clusters").iter().map(|c| {
                let c = short(c);
                let (svcs, tasks) = if c == CLUSTER { (SVCS.len(), running().len()) } else if c == "data-jobs" { (2, 3) } else { (5, 7) };
                json!({
                    "clusterName": c, "clusterArn": format!("arn:aws:ecs:{r}:{ACCT}:cluster/{c}"), "status": "ACTIVE",
                    "activeServicesCount": svcs, "runningTasksCount": tasks, "pendingTasksCount": 0, "registeredContainerInstancesCount": 0,
                    "capacityProviders": ["FARGATE", "FARGATE_SPOT"], "settings": [{"name": "containerInsights", "value": "enabled"}],
                })
            }).collect();
            json!({"clusters": cs, "failures": []})
        }
        ("ecs", "list-services") => {
            let names: Vec<String> = if short(&a.one("--cluster").unwrap_or_default()) == CLUSTER { SVCS.iter().map(|s| s.name.to_string()).collect() } else { vec![] };
            json!({"serviceArns": names.iter().map(|n| format!("arn:aws:ecs:{r}:{ACCT}:service/{CLUSTER}/{n}")).collect::<Vec<_>>()})
        }
        ("ecs", "describe-services") => {
            let s: Vec<Value> = a.all("--services").iter().filter_map(|n| svc(short(n))).map(|s| service_json(r, s)).collect();
            json!({"services": s, "failures": []})
        }
        ("ecs", "list-tasks") => {
            let (want_svc, status) = (a.one("--service-name"), a.one("--desired-status").unwrap_or("RUNNING".into()));
            let arns: Vec<String> = tasks()
                .iter()
                .filter(|t| want_svc.as_deref().is_none_or(|w| short(w) == t.svc.name) && t.stopped == (status == "STOPPED"))
                .map(|t| format!("arn:aws:ecs:{r}:{ACCT}:task/{CLUSTER}/{}", t.id))
                .collect();
            json!({"taskArns": arns})
        }
        ("ecs", "describe-tasks") => {
            let all = tasks();
            let ts: Vec<Value> = a.all("--tasks").iter().filter_map(|id| all.iter().find(|t| t.id == short(id))).map(|t| task(r, t)).collect();
            json!({"tasks": ts, "failures": []})
        }
        ("ecs", "list-task-definition-families") => json!({"families": SVCS.iter().map(|s| s.name).collect::<Vec<_>>()}),
        ("ecs", "list-task-definitions") => {
            let fam = a.one("--family-prefix").unwrap_or_default();
            let mut arns: Vec<String> = SVCS
                .iter()
                .filter(|s| fam.is_empty() || s.name == fam)
                .flat_map(|s| (s.rev.saturating_sub(5).max(1)..=s.rev).map(move |v| format!("arn:aws:ecs:{r}:{ACCT}:task-definition/{}:{v}", s.name)))
                .collect();
            if a.one("--sort").as_deref() == Some("DESC") {
                arns.reverse();
            }
            json!({"taskDefinitionArns": arns})
        }
        ("ecs", "describe-task-definition") => {
            let td = a.one("--task-definition").unwrap_or_default();
            let (fam, rev) = short(&td).split_once(':').map(|(f, v)| (f.to_string(), v.parse().unwrap_or(1))).unwrap_or((short(&td).to_string(), 1));
            let s = svc(&fam).ok_or_else(|| AwsError::Api(format!("Unable to describe task definition {td}")))?;
            json!({"taskDefinition": taskdef(r, s, rev)})
        }

        ("logs", "describe-log-groups") => {
            let mut g: Vec<Value> = SVCS
                .iter()
                .map(|s| json!({"logGroupName": group(s.name), "retentionInDays": 30, "storedBytes": 3_000_000_000u64 / (s.cpu as u64 / 256), "creationTime": ms_ago(Duration::days(400))}))
                .collect();
            for n in ["/aws/lambda/thumbnail-generator", "/aws/lambda/stripe-webhook", "/aws/rds/cluster/shop-db/postgresql"] {
                g.push(json!({"logGroupName": n, "retentionInDays": 14, "storedBytes": 81_000_000, "creationTime": ms_ago(Duration::days(200))}));
            }
            json!({"logGroups": g})
        }
        ("logs", "describe-log-streams") => {
            let g = a.one("--log-group-name").unwrap_or_default();
            let st: Vec<Value> = tasks()
                .iter()
                .filter(|t| group(t.svc.name) == g)
                .map(|t| json!({
                    "logStreamName": format!("ecs/{}/{}", t.svc.name, t.id), "creationTime": ms_ago(t.age),
                    "firstEventTimestamp": ms_ago(t.age), "lastEventTimestamp": if t.stopped { ms_ago(t.age / 2) } else { Utc::now().timestamp_millis() },
                }))
                .collect();
            json!({"logStreams": st})
        }
        ("logs", "filter-log-events" | "get-log-events") => {
            let g = a.one("--log-group-name").unwrap_or_default();
            let mut streams = a.all("--log-stream-names");
            if let Some(one) = a.one("--log-stream-name") {
                streams.push(one);
            }
            if streams.is_empty() {
                let prefix = a.one("--log-stream-name-prefix").unwrap_or_default();
                streams = tasks()
                    .iter()
                    .filter(|t| !t.stopped && group(t.svc.name) == g)
                    .map(|t| format!("ecs/{}/{}", t.svc.name, t.id))
                    .filter(|s| s.starts_with(&prefix))
                    .collect();
            }
            let now = Utc::now().timestamp_millis();
            let start = a.one("--start-time").and_then(|s| s.parse().ok()).unwrap_or(now - 60_000).max(now - 30 * 60_000);
            let max = a.one("--max-items").or(a.one("--limit")).and_then(|m| m.parse().ok()).unwrap_or(10_000);
            let (ev, cut) = events(&streams, start, now, a.one("--filter-pattern").as_deref(), max);
            let mut v = json!({"events": ev});
            if cut {
                v["NextToken"] = json!("more");
            }
            v
        }

        ("elbv2", "describe-load-balancers") => json!({"LoadBalancers": [lb(r, "shop-public", "internet-facing"), lb(r, "shop-internal", "internal")]}),
        ("elbv2", "describe-target-groups") => {
            let only = a.one("--load-balancer-arn");
            let tgs: Vec<Value> = SVCS
                .iter()
                .filter(|s| s.port.is_some())
                .map(|s| target_group(r, s))
                .filter(|tg| only.as_ref().is_none_or(|l| tg["LoadBalancerArns"][0] == json!(l)))
                .collect();
            json!({"TargetGroups": tgs})
        }
        ("elbv2", "describe-target-health") => {
            let arn = a.one("--target-group-arn").unwrap_or_default();
            let name = arn.split('/').nth(1).unwrap_or_default().trim_end_matches("-tg").to_string();
            let s = svc(&name).ok_or_else(|| AwsError::Api(format!("TargetGroupNotFound: {arn}")))?;
            let d: Vec<Value> = tasks()
                .iter()
                .filter(|t| t.svc.name == s.name && (!t.stopped || t.draining))
                .map(|t| {
                    let (state, reason) = if t.draining { ("draining", Some("Target.DeregistrationInProgress")) } else if t.fresh { ("initial", Some("Elb.RegistrationInProgress")) } else { ("healthy", None) };
                    json!({"Target": {"Id": t.ip(), "Port": s.port, "AvailabilityZone": t.az(r)}, "HealthCheckPort": s.port.map(|p| p.to_string()),
                           "TargetHealth": {"State": state, "Reason": reason}})
                })
                .collect();
            json!({"TargetHealthDescriptions": d})
        }
        ("elbv2", "describe-target-group-attributes") => json!({"Attributes": [
            {"Key": "deregistration_delay.timeout_seconds", "Value": "30"}, {"Key": "stickiness.enabled", "Value": "false"},
            {"Key": "load_balancing.algorithm.type", "Value": "least_outstanding_requests"}, {"Key": "slow_start.duration_seconds", "Value": "0"},
        ]}),

        ("application-autoscaling", "describe-scalable-targets") => {
            let x: Vec<Value> = scaled()
                .map(|(s, min, max)| json!({
                    "ServiceNamespace": "ecs", "ResourceId": format!("service/{CLUSTER}/{}", s.name), "ScalableDimension": "ecs:service:DesiredCount",
                    "MinCapacity": min, "MaxCapacity": max, "CreationTime": ago(Duration::days(250)),
                    "SuspendedState": {"DynamicScalingInSuspended": false, "DynamicScalingOutSuspended": false, "ScheduledScalingSuspended": false},
                }))
                .collect();
            json!({"ScalableTargets": x})
        }
        ("application-autoscaling", "describe-scaling-policies") => {
            let res = a.one("--resource-id");
            let x: Vec<Value> = scaled()
                .filter(|(s, ..)| res.as_ref().is_none_or(|r| r.ends_with(&format!("/{}", s.name))))
                .map(|(s, ..)| json!({
                    "PolicyARN": format!("arn:aws:autoscaling:{r}:{ACCT}:scalingPolicy:demo:resource/ecs/service/{CLUSTER}/{}:policyName/{}-cpu", s.name, s.name),
                    "PolicyName": format!("{}-cpu", s.name), "ResourceId": format!("service/{CLUSTER}/{}", s.name), "PolicyType": "TargetTrackingScaling",
                    "ServiceNamespace": "ecs", "ScalableDimension": "ecs:service:DesiredCount",
                    "TargetTrackingScalingPolicyConfiguration": {"TargetValue": 60.0, "ScaleInCooldown": 300, "ScaleOutCooldown": 60, "DisableScaleIn": false,
                        "PredefinedMetricSpecification": {"PredefinedMetricType": "ECSServiceAverageCPUUtilization"}},
                }))
                .collect();
            json!({"ScalingPolicies": x})
        }
        ("application-autoscaling", "describe-scaling-activities") => {
            let res = a.one("--resource-id").unwrap_or_default();
            let x: Vec<Value> = [(25, 8, "monitor alarm TargetTracking-api-AlarmHigh in state ALARM triggered policy api-cpu"), (190, 6, "monitor alarm TargetTracking-api-AlarmLow in state ALARM triggered policy api-cpu"), (600, 7, "monitor alarm TargetTracking-api-AlarmHigh in state ALARM triggered policy api-cpu")]
                .iter()
                .enumerate()
                .map(|(i, (min, n, cause))| json!({
                    "ActivityId": format!("0d1e2f3a-0000-4000-8000-00000000000{i}"), "ResourceId": res,
                    "Description": format!("Setting desired count to {n}."), "Cause": cause, "StatusCode": "Successful",
                    "StatusMessage": format!("Successfully set desired count to {n}. Change successfully fulfilled by ecs."),
                    "StartTime": ago(Duration::minutes(*min)), "EndTime": ago(Duration::minutes(*min) - Duration::seconds(40)),
                }))
                .collect();
            json!({"ScalingActivities": x})
        }

        ("cloudwatch", "get-metric-data") => {
            let qs: Vec<Value> = a.one("--metric-data-queries").and_then(|q| serde_json::from_str(&q).ok()).unwrap_or_default();
            let parse = |k: &str| a.one(k).and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()).map(|t| t.with_timezone(&Utc));
            let (start, end) = (parse("--start-time").unwrap_or(Utc::now() - Duration::hours(3)), parse("--end-time").unwrap_or(Utc::now()));
            let x: Vec<Value> = qs
                .iter()
                .map(|q| {
                    let m = &q["MetricStat"];
                    let period = m["Period"].as_i64().unwrap_or(300);
                    let (metric, stat) = (m["Metric"]["MetricName"].as_str().unwrap_or_default(), m["Stat"].as_str().unwrap_or_default());
                    let seed = hash(&m["Metric"]["Dimensions"].to_string());
                    let mut t = start;
                    let (mut times, mut vals) = (vec![], vec![]);
                    while t <= end {
                        times.push(json!(t.to_rfc3339_opts(SecondsFormat::Millis, true)));
                        vals.push(json!(metric_value(metric, stat, seed, (t - start).num_seconds() as f64 / period as f64)));
                        t += Duration::seconds(period);
                    }
                    json!({"Id": q["Id"], "Label": q["Label"], "StatusCode": "Complete", "Timestamps": times, "Values": vals})
                })
                .collect();
            json!({"MetricDataResults": x})
        }

        ("ecr", "describe-repositories") => json!({"repositories": SVCS.iter().map(|s| json!({
            "repositoryName": s.name, "repositoryArn": format!("arn:aws:ecr:{r}:{ACCT}:repository/{}", s.name),
            "repositoryUri": format!("{ACCT}.dkr.ecr.{r}.amazonaws.com/{}", s.name), "imageTagMutability": "IMMUTABLE",
            "createdAt": ago(Duration::days(500)), "imageScanningConfiguration": {"scanOnPush": true},
        })).collect::<Vec<_>>()}),
        ("ecr", "describe-images") => {
            let repo = a.one("--repository-name").unwrap_or_default();
            let s = svc(&repo).ok_or_else(|| AwsError::Api(format!("RepositoryNotFoundException: {repo}")))?;
            let x: Vec<Value> = (0..8u32)
                .map(|i| json!({
                    "imageDigest": format!("sha256:{}", hex(&format!("{repo}{i}"), 64)), "imageTags": [tag(s.rev - i)],
                    "imageSizeInBytes": 180_000_000 + (hash(&format!("{repo}{i}")) % 40_000_000), "imagePushedAt": ago(Duration::hours(20 * i as i64 + 3)),
                    "lastRecordedPullTime": ago(Duration::minutes(if i < 2 { 4 } else { 60 * 24 * i as i64 })),
                }))
                .collect();
            json!({"imageDetails": x})
        }

        ("sqs", "list-queues") => {
            let q: Vec<String> = ["orders", "orders-dlq", "emails", "image-jobs", "search-reindex"].iter().map(|q| format!("https://sqs.{r}.amazonaws.com/{ACCT}/{q}")).collect();
            json!({"QueueUrls": q})
        }
        ("sqs", "get-queue-attributes") => {
            let q = short(&a.one("--queue-url").unwrap_or_default()).to_string();
            let (vis, flight, delayed) = match q.as_str() { "orders" => (12, 40, 0), "orders-dlq" => (3, 0, 0), "image-jobs" => (1840, 2, 0), "emails" => (0, 6, 25), _ => (0, 0, 0) };
            let mut at = json!({
                "QueueArn": format!("arn:aws:sqs:{r}:{ACCT}:{q}"), "ApproximateNumberOfMessages": vis.to_string(),
                "ApproximateNumberOfMessagesNotVisible": flight.to_string(), "ApproximateNumberOfMessagesDelayed": delayed.to_string(),
                "MessageRetentionPeriod": if q.ends_with("dlq") { "1209600" } else { "345600" }, "VisibilityTimeout": "30",
            });
            if q == "orders" {
                at["RedrivePolicy"] = json!(format!("{{\"deadLetterTargetArn\":\"arn:aws:sqs:{r}:{ACCT}:orders-dlq\",\"maxReceiveCount\":5}}"));
            }
            json!({"Attributes": at})
        }

        ("rds", "describe-db-clusters") => json!({"DBClusters": [{
            "DBClusterIdentifier": "shop-db", "DBClusterArn": format!("arn:aws:rds:{r}:{ACCT}:cluster:shop-db"), "Engine": "aurora-postgresql",
            "EngineVersion": "16.4", "Status": "available", "MultiAZ": true, "ClusterCreateTime": ago(Duration::days(700)),
            "Endpoint": format!("shop-db.cluster-c0demo.{r}.rds.amazonaws.com"), "ReaderEndpoint": format!("shop-db.cluster-ro-c0demo.{r}.rds.amazonaws.com"),
            "DBClusterMembers": [{"DBInstanceIdentifier": "shop-db-1", "IsClusterWriter": true}, {"DBInstanceIdentifier": "shop-db-2", "IsClusterWriter": false}],
        }]}),
        ("rds", "describe-db-instances") => {
            let i = |id: &str, class: &str, az: &str, cl: Option<&str>, engine: &str| json!({
                "DBInstanceIdentifier": id, "DBInstanceArn": format!("arn:aws:rds:{r}:{ACCT}:db:{id}"), "Engine": engine, "EngineVersion": "16.4",
                "DBInstanceClass": class, "DBInstanceStatus": "available", "AvailabilityZone": format!("{r}{az}"),
                "InstanceCreateTime": ago(Duration::days(300)), "DBClusterIdentifier": cl, "MultiAZ": false,
            });
            let mut analytics = i("analytics", "db.t4g.large", "c", None, "postgres");
            // A class and storage change being applied, for the PENDING column and its live `d` page.
            analytics["DBInstanceStatus"] = json!("modifying");
            analytics["AllocatedStorage"] = json!(100);
            analytics["StorageType"] = json!("gp3");
            analytics["Endpoint"] = json!({"Address": format!("analytics.c0demo.{r}.rds.amazonaws.com"), "Port": 5432});
            analytics["PreferredMaintenanceWindow"] = json!("sun:03:00-sun:03:30");
            analytics["PendingModifiedValues"] = json!({"DBInstanceClass": "db.t4g.xlarge", "AllocatedStorage": 200});
            let all = [i("shop-db-1", "db.r6g.xlarge", "a", Some("shop-db"), "aurora-postgresql"), i("shop-db-2", "db.r6g.xlarge", "b", Some("shop-db"), "aurora-postgresql"), analytics];
            let all: Vec<Value> = all.into_iter().filter(|x| a.one("--db-instance-identifier").is_none_or(|id| x["DBInstanceIdentifier"] == json!(id))).collect();
            json!({"DBInstances": all})
        }
        ("rds", "describe-events") => {
            let id = a.one("--source-identifier").unwrap_or_default();
            let e = |m: u32, msg: &str| json!({"SourceIdentifier": id, "SourceType": "db-instance", "Message": msg, "Date": ago(Duration::minutes(m.into()))});
            json!({"Events": if id == "analytics" {
                vec![e(600, "Backing up DB instance"), e(597, "Finished DB Instance backup"), e(7, "Applying modification to database instance class"), e(5, "DB instance shutdown")]
            } else {
                vec![e(600, "Backing up DB instance"), e(597, "Finished DB Instance backup")]
            }})
        }

        ("rds", "describe-pending-maintenance-actions") => {
            let arn = a.one("--resource-identifier").unwrap_or_default();
            json!({"PendingMaintenanceActions": if arn.ends_with(":db:analytics") {
                vec![json!({"ResourceIdentifier": arn, "PendingMaintenanceActionDetails": [{
                    "Action": "system-update", "Description": "New Operating System update is available",
                    "AutoAppliedAfterDate": ago(Duration::days(-9)),
                }]})]
            } else {
                vec![]
            }})
        }

        ("cloudwatch", "describe-alarms") => {
            let m = |name: &str, state: &str, since: Duration, ns: &str, metric: &str, dims: Value, stat: &str, op: &str, th: f64, reason: &str| json!({
                "AlarmName": name, "AlarmArn": format!("arn:aws:cloudwatch:{r}:{ACCT}:alarm:{name}"), "StateValue": state, "StateReason": reason,
                "StateUpdatedTimestamp": ago(since), "Namespace": ns, "MetricName": metric, "Dimensions": dims, "Statistic": stat,
                "Period": 60, "EvaluationPeriods": 3, "DatapointsToAlarm": 2, "Threshold": th, "ComparisonOperator": op,
                "ActionsEnabled": true, "AlarmActions": [format!("arn:aws:sns:{r}:{ACCT}:oncall-page")],
            });
            let svc = |s: &str| json!([{"Name": "ClusterName", "Value": CLUSTER}, {"Name": "ServiceName", "Value": s}]);
            json!({"MetricAlarms": [
                m("image-resizer-memory-high", "ALARM", Duration::minutes(14), "AWS/ECS", "MemoryUtilization", svc(CRASHING), "Maximum",
                  "GreaterThanThreshold", 90.0, "Threshold Crossed: 2 out of the last 3 datapoints [99.8, 100.0] were greater than the threshold (90.0)."),
                m("stripe-webhook-errors", "ALARM", Duration::minutes(6), "AWS/Lambda", "Errors", json!([{"Name": "FunctionName", "Value": "stripe-webhook"}]), "Sum",
                  "GreaterThanOrEqualToThreshold", 5.0, "Threshold Crossed: 2 out of the last 3 datapoints [12.0, 9.0] were greater than or equal to the threshold (5.0)."),
                m("analytics-free-storage-low", "INSUFFICIENT_DATA", Duration::minutes(5), "AWS/RDS", "FreeStorageSpace", json!([{"Name": "DBInstanceIdentifier", "Value": "analytics"}]), "Minimum",
                  "LessThanThreshold", 10e9, "Insufficient Data: 1 datapoint was unknown."),
                m("api-5xx", "OK", Duration::days(3), "AWS/ApplicationELB", "HTTPCode_Target_5XX_Count", json!([{"Name": "LoadBalancer", "Value": "app/shop-public/50dc6c495c0c9188"}]), "Sum",
                  "GreaterThanThreshold", 50.0, "Threshold Crossed: no datapoints were greater than the threshold (50.0)."),
                m("orders-dlq-not-empty", "OK", Duration::days(11), "AWS/SQS", "ApproximateNumberOfMessagesVisible", json!([{"Name": "QueueName", "Value": "orders-dlq"}]), "Maximum",
                  "GreaterThanThreshold", 0.0, "Threshold Crossed: no datapoints were greater than the threshold (0.0)."),
            ], "CompositeAlarms": []})
        }

        ("cloudtrail", "lookup-events") => {
            let attr = a.one("--lookup-attributes").unwrap_or_default();
            let resource = attr.split_once("AttributeKey=ResourceName,AttributeValue=").map(|x| x.1.to_string());
            let e = |mins: i64, who: &str, name: &str, src: &str, res: &str, params: Value, err: Option<&str>| {
                let mut detail = json!({
                    "eventName": name, "eventSource": format!("{src}.amazonaws.com"), "awsRegion": r, "sourceIPAddress": "203.0.113.7",
                    "userIdentity": {"type": "AssumedRole", "arn": format!("arn:aws:sts::{ACCT}:assumed-role/PlatformAdmin/{who}"),
                                     "sessionContext": {"sessionIssuer": {"userName": "PlatformAdmin"}}},
                    "requestParameters": params,
                });
                if let Some(code) = err {
                    detail["errorCode"] = json!(code);
                    detail["errorMessage"] = json!("User is not authorized to perform this operation");
                }
                json!({"EventId": hex(&format!("{name}{mins}"), 32), "EventName": name, "ReadOnly": "false", "EventTime": ago(Duration::minutes(mins)),
                       "EventSource": format!("{src}.amazonaws.com"), "Username": who,
                       "Resources": [{"ResourceType": format!("AWS::{src}"), "ResourceName": res}], "CloudTrailEvent": detail.to_string()})
            };
            let all = vec![
                e(6, "dana", "UpdateFunctionCode20150331v2", "lambda", "stripe-webhook", json!({"functionName": "stripe-webhook", "publish": true}), None),
                e(9, "dana", "UpdateFunctionConfiguration20150331v2", "lambda", "stripe-webhook",
                  json!({"functionName": "stripe-webhook", "environment": {"variables": {"STRIPE_KEY": "sk_live_demo"}}}), None),
                e(11, "sam", "ModifyDBInstance", "rds", "analytics",
                  json!({"dBInstanceIdentifier": "analytics", "dBInstanceClass": "db.t4g.xlarge", "allocatedStorage": 200, "applyImmediately": true}), None),
                e(26, "ci-deploy", "UpdateService", "ecs", "api", json!({"cluster": CLUSTER, "service": "api", "taskDefinition": "api:43"}), None),
                e(40, "intern", "DeleteDBInstance", "rds", "analytics", json!({"dBInstanceIdentifier": "analytics"}), Some("AccessDenied")),
                e(300, "sam", "PutMetricAlarm", "monitoring", "image-resizer-memory-high", json!({"alarmName": "image-resizer-memory-high", "threshold": 90}), None),
            ];
            json!({"Events": all.into_iter().filter(|x| resource.as_ref().is_none_or(|r| x["Resources"][0]["ResourceName"] == json!(r))).collect::<Vec<_>>()})
        }

        ("lambda", "list-functions") => json!({"Functions": functions(r)}),
        ("lambda", "get-function-configuration") => {
            let name = a.one("--function-name").unwrap_or_default();
            let mut f = functions(r).into_iter().find(|f| f["FunctionName"] == json!(name)).ok_or_else(|| AwsError::Api(format!("function {name} not found")))?;
            // stripe-webhook is mid-deploy, for the STATUS column and its live `d` page.
            let updating = name == "stripe-webhook";
            f["State"] = json!("Active");
            f["LastUpdateStatus"] = json!(if updating { "InProgress" } else { "Successful" });
            if updating {
                f["LastUpdateStatusReason"] = json!("The function is being created.");
                f["LastUpdateStatusReasonCode"] = json!("Creating");
            }
            f["CodeSha256"] = json!(hex(&name, 43));
            f["Version"] = json!("$LATEST");
            f["PackageType"] = json!("Zip");
            f["Role"] = json!(format!("arn:aws:iam::{ACCT}:role/{name}-role"));
            f
        }

        _ => return Err(AwsError::Api(format!("e9s does not call aws {service} {op}"))),
    })
}

fn functions(r: &str) -> Vec<Value> {
    let f = |n: &str, rt: &str, mem: u32, t: u32, age: Duration| json!({
        "FunctionName": n, "FunctionArn": format!("arn:aws:lambda:{r}:{ACCT}:function:{n}"), "Runtime": rt, "MemorySize": mem, "Timeout": t,
        "CodeSize": 4_200_000, "LastModified": ago(age), "Architectures": ["arm64"], "Handler": "index.handler",
        "LoggingConfig": {"LogGroup": format!("/aws/lambda/{n}")},
    });
    vec![
        f("thumbnail-generator", "nodejs22.x", 1024, 30, Duration::days(12)),
        f("stripe-webhook", "python3.13", 256, 10, Duration::minutes(2)),
        f("nightly-export", "python3.13", 2048, 900, Duration::days(12)),
    ]
}

struct Args<'a>(&'a [String]);

impl Args<'_> {
    fn all(&self, flag: &str) -> Vec<String> {
        self.0.iter().skip_while(|a| *a != flag).skip(1).take_while(|a| !a.starts_with("--")).cloned().collect()
    }
    fn one(&self, flag: &str) -> Option<String> {
        self.all(flag).into_iter().next()
    }
}

fn short(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn svc(name: &str) -> Option<&'static Svc> {
    SVCS.iter().find(|s| s.name == name)
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn hex(seed: &str, len: usize) -> String {
    let mut out = String::new();
    let mut i = 0;
    while out.len() < len {
        out += &format!("{:016x}", hash(&format!("{seed}#{i}")));
        i += 1;
    }
    out.truncate(len);
    out
}

fn ago(d: Duration) -> String {
    (Utc::now() - d).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn ms_ago(d: Duration) -> i64 {
    (Utc::now() - d).timestamp_millis()
}

fn tag(rev: u32) -> String {
    format!("v2.{rev}.0-{}", &hex(&rev.to_string(), 7))
}

fn group(svc: &str) -> String {
    format!("/ecs/{CLUSTER}/{svc}")
}

fn deployment_id(s: &Svc, new: bool) -> String {
    format!("ecs-svc/{}", &hex(&format!("{}{new}", s.name), 19).chars().map(|c| if c.is_ascii_digit() { c } else { '7' }).collect::<String>())
}

struct Task {
    svc: &'static Svc,
    id: String,
    n: usize,
    rev: u32,
    stopped: bool,
    /// Drained from the old generation, still deregistering from the target group.
    draining: bool,
    /// Just started by the rollout, still registering.
    fresh: bool,
    age: Duration,
}

impl Task {
    fn ip(&self) -> String {
        format!("10.0.{}.{}", 10 + self.n % 3, 20 + hash(&self.id) % 200)
    }
    fn az(&self, r: &str) -> String {
        format!("{r}{}", ["a", "b", "c"][self.n % 3])
    }
}

fn tasks() -> Vec<Task> {
    let mut v = vec![];
    for s in SVCS {
        let t = |n: usize, rev: u32, stopped: bool, age: Duration| Task {
            svc: s, id: hex(&format!("{}-{n}", s.name), 32), n, rev, stopped, draining: false, fresh: false, age,
        };
        match s.name {
            ROLLING => {
                for n in 0..s.desired {
                    let new = n < ROLL_NEW;
                    let mut x = t(n, if new { s.rev } else { s.rev - 1 }, false, if new { Duration::minutes(3 + n as i64) } else { Duration::days(2) + Duration::hours(n as i64) });
                    x.fresh = new && n == 0;
                    v.push(x);
                }
                for n in 0..ROLL_NEW {
                    let mut x = t(50 + n, s.rev - 1, true, Duration::days(2));
                    x.draining = n == 0;
                    v.push(x);
                }
            }
            CRASHING => {
                v.push(t(0, s.rev, false, Duration::hours(5)));
                for n in 0..4 {
                    v.push(t(100 + n, s.rev, true, Duration::minutes(4 + 13 * n as i64)));
                }
            }
            _ => (0..s.desired).for_each(|n| v.push(t(n, s.rev, false, Duration::hours(6 + 7 * n as i64 + s.rev as i64)))),
        }
    }
    v
}

fn running() -> Vec<Task> {
    tasks().into_iter().filter(|t| !t.stopped).collect()
}

fn task(r: &str, t: &Task) -> Value {
    let s = t.svc;
    let created = Utc::now() - t.age;
    let at = |d: i64| (created + Duration::seconds(d)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let crashed = t.stopped && s.name == CRASHING;
    let mut x = json!({
        "taskArn": format!("arn:aws:ecs:{r}:{ACCT}:task/{CLUSTER}/{}", t.id), "clusterArn": format!("arn:aws:ecs:{r}:{ACCT}:cluster/{CLUSTER}"),
        "group": format!("service:{}", s.name), "startedBy": deployment_id(s, t.rev == s.rev),
        "taskDefinitionArn": format!("arn:aws:ecs:{r}:{ACCT}:task-definition/{}:{}", s.name, t.rev),
        "lastStatus": if t.stopped { "STOPPED" } else { "RUNNING" }, "desiredStatus": if t.stopped { "STOPPED" } else { "RUNNING" },
        "healthStatus": if t.stopped { "UNKNOWN" } else if t.fresh { "UNKNOWN" } else { "HEALTHY" },
        "launchType": "FARGATE", "platformVersion": "1.4.0", "availabilityZone": t.az(r), "connectivity": "CONNECTED",
        "cpu": s.cpu.to_string(), "memory": s.mem.to_string(), "createdAt": at(0), "pullStartedAt": at(4), "pullStoppedAt": at(19), "startedAt": at(27),
        "attachments": [{"id": hex(&t.id, 8), "type": "ElasticNetworkInterface", "status": "ATTACHED",
            "details": [{"name": "subnetId", "value": "subnet-0demo1"}, {"name": "privateIPv4Address", "value": t.ip()}]}],
        "containers": containers(s).iter().enumerate().map(|(i, c)| json!({
            "name": c, "image": image(r, s, t.rev, i), "lastStatus": if t.stopped { "STOPPED" } else { "RUNNING" },
            "exitCode": if crashed && i == 0 { Some(137) } else if t.stopped { Some(0) } else { None },
            "reason": if crashed && i == 0 { Some("OutOfMemoryError: Container killed due to memory usage") } else { None },
            "healthStatus": if i == 0 && !t.stopped && !t.fresh { "HEALTHY" } else { "UNKNOWN" },
            "runtimeId": format!("{}-{}", t.id, hex(c, 10)), "networkInterfaces": [{"privateIpv4Address": t.ip()}],
        })).collect::<Vec<_>>(),
    });
    if t.stopped {
        let stopped_at = if crashed { at(150) } else { (Utc::now() - Duration::minutes(2 + t.n as i64 % 3)).to_rfc3339_opts(SecondsFormat::Millis, true) };
        let (code, why) = if crashed {
            ("EssentialContainerExited", "Essential container in task exited".to_string())
        } else {
            ("ServiceSchedulerInitiated", format!("Scaling activity initiated by (deployment {})", short(&deployment_id(s, true))))
        };
        for k in ["stoppingAt", "stoppedAt", "executionStoppedAt"] {
            x[k] = json!(stopped_at);
        }
        x["stopCode"] = json!(code);
        x["stoppedReason"] = json!(why);
    }
    x
}

fn containers(s: &Svc) -> Vec<&'static str> {
    match s.name {
        "web" | "api" => vec![s.name, "otel-collector"],
        _ => vec![s.name],
    }
}

fn image(r: &str, s: &Svc, rev: u32, i: usize) -> String {
    if i == 0 { format!("{ACCT}.dkr.ecr.{r}.amazonaws.com/{}:{}", s.name, tag(rev)) } else { "public.ecr.aws/aws-observability/aws-otel-collector:v0.43.1".into() }
}

fn service_json(r: &str, s: &Svc) -> Value {
    let all = tasks();
    let run = all.iter().filter(|t| t.svc.name == s.name && !t.stopped).count();
    let td = |rev: u32| format!("arn:aws:ecs:{r}:{ACCT}:task-definition/{}:{rev}", s.name);
    let dep = |new: bool, rev: u32, running: usize, state: &str, age: Duration| json!({
        "id": deployment_id(s, new), "status": if new { "PRIMARY" } else { "ACTIVE" }, "taskDefinition": td(rev),
        "desiredCount": if new { s.desired } else { s.desired - ROLL_NEW }, "runningCount": running, "pendingCount": 0,
        "failedTasks": if s.name == CRASHING { 4 } else { 0 }, "rolloutState": state,
        "rolloutStateReason": if state == "COMPLETED" { "ECS deployment completed." } else { "ECS deployment in progress." },
        "createdAt": ago(age), "updatedAt": ago(Duration::minutes(1)),
    });
    let ev = |min: i64, m: String| json!({"id": hex(&m, 36), "createdAt": ago(Duration::minutes(min)), "message": format!("(service {}) {m}", s.name)});
    let (deployments, events) = match s.name {
        ROLLING => (
            vec![dep(true, s.rev, ROLL_NEW, "IN_PROGRESS", Duration::minutes(6)), dep(false, s.rev - 1, s.desired - ROLL_NEW, "COMPLETED", Duration::days(2))],
            vec![
                ev(1, format!("registered 1 targets in (target-group arn:aws:elasticloadbalancing:{r}:{ACCT}:targetgroup/api-tg/6d0ecf831eec9f09)")),
                ev(2, format!("has stopped 1 running tasks: (task {}).", hex("api-50", 32))),
                ev(3, format!("has started 1 tasks: (task {}).", hex("api-0", 32))),
                ev(5, format!("has started 2 tasks: (task {}) (task {}).", hex("api-1", 32), hex("api-2", 32))),
                ev(6, format!("updated computedDesiredCount for taskSet {} to 8.", deployment_id(s, true))),
                ev(240, "has reached a steady state.".into()),
            ],
        ),
        CRASHING => (
            vec![dep(true, s.rev, 1, "IN_PROGRESS", Duration::hours(5))],
            vec![
                ev(4, format!("has started 1 tasks: (task {}).", hex("image-resizer-100", 32))),
                ev(6, format!("task {} stopped: Essential container in task exited (OutOfMemoryError)", hex("image-resizer-101", 32))),
                ev(17, format!("is unable to consistently start tasks successfully. For more information, see the Troubleshooting section of the Amazon ECS Developer Guide.")),
                ev(30, format!("has started 1 tasks: (task {}).", hex("image-resizer-102", 32))),
            ],
        ),
        _ => (vec![dep(true, s.rev, run, "COMPLETED", Duration::days(3))], vec![ev(180 + s.rev as i64, "has reached a steady state.".into())]),
    };
    json!({
        "serviceName": s.name, "serviceArn": format!("arn:aws:ecs:{r}:{ACCT}:service/{CLUSTER}/{}", s.name),
        "clusterArn": format!("arn:aws:ecs:{r}:{ACCT}:cluster/{CLUSTER}"), "status": "ACTIVE",
        "desiredCount": s.desired, "runningCount": run, "pendingCount": if s.name == CRASHING { 1 } else { 0 },
        "launchType": "FARGATE", "platformVersion": "LATEST", "taskDefinition": td(s.rev), "schedulingStrategy": "REPLICA",
        "createdAt": ago(Duration::days(400)), "healthCheckGracePeriodSeconds": s.port.map(|_| 60),
        "deploymentController": {"type": "ECS"},
        "deploymentConfiguration": {"maximumPercent": 200, "minimumHealthyPercent": 100, "deploymentCircuitBreaker": {"enable": true, "rollback": true}},
        "networkConfiguration": {"awsvpcConfiguration": {"subnets": ["subnet-0demo1", "subnet-0demo2", "subnet-0demo3"], "securityGroups": ["sg-0demo"], "assignPublicIp": "DISABLED"}},
        "loadBalancers": s.port.map(|p| vec![json!({"targetGroupArn": target_group(r, s)["TargetGroupArn"], "containerName": s.name, "containerPort": p})]).unwrap_or_default(),
        "deployments": deployments, "events": events,
    })
}

fn taskdef(r: &str, s: &Svc, rev: u32) -> Value {
    let env: &[&str] = &["LOG_LEVEL", "DATABASE_URL", "REDIS_URL", "OTEL_EXPORTER_OTLP_ENDPOINT"];
    let secrets: &[&str] = if s.name == "checkout" || s.name == "payments-worker" { &["STRIPE_API_KEY", "DB_PASSWORD"] } else { &["DB_PASSWORD"] };
    let defs: Vec<Value> = containers(s)
        .iter()
        .enumerate()
        .map(|(i, c)| json!({
            "name": c, "image": image(r, s, rev, i), "cpu": 0, "essential": i == 0,
            "memoryReservation": if i == 0 { None } else { Some(128) },
            "command": if i == 0 { None } else { Some(vec!["--config=/etc/ecs/ecs-default-config.yaml"]) },
            "portMappings": if i == 0 { s.port.map(|p| vec![json!({"containerPort": p, "hostPort": p, "protocol": "tcp"})]).unwrap_or_default() } else { vec![] },
            "healthCheck": (i == 0 && s.port.is_some()).then(|| json!({"command": ["CMD-SHELL", format!("curl -fs localhost:{}/healthz || exit 1", s.port.unwrap())], "interval": 15, "timeout": 5, "retries": 3, "startPeriod": 20})),
            "logConfiguration": {"logDriver": "awslogs", "options": {"awslogs-group": group(s.name), "awslogs-region": r, "awslogs-stream-prefix": "ecs"}},
            "environment": if i == 0 { env.iter().map(|n| json!({"name": n, "value": REDACTED})).collect() } else { vec![] },
            "secrets": if i == 0 { secrets.iter().map(|n| json!({"name": n, "valueFrom": REDACTED})).collect() } else { vec![] },
            "dependsOn": if i == 0 && containers(s).len() > 1 { vec![json!({"containerName": "otel-collector", "condition": "START"})] } else { vec![] },
        }))
        .collect();
    json!({
        "taskDefinitionArn": format!("arn:aws:ecs:{r}:{ACCT}:task-definition/{}:{rev}", s.name), "family": s.name, "revision": rev,
        "status": "ACTIVE", "cpu": s.cpu.to_string(), "memory": s.mem.to_string(), "networkMode": "awsvpc",
        "registeredAt": ago(Duration::hours(20 * (s.rev - rev) as i64 + 7)), "requiresCompatibilities": ["FARGATE"], "containerDefinitions": defs,
    })
}

fn lb(r: &str, name: &str, scheme: &str) -> Value {
    json!({
        "LoadBalancerArn": format!("arn:aws:elasticloadbalancing:{r}:{ACCT}:loadbalancer/app/{name}/{}", hex(name, 16)), "LoadBalancerName": name,
        "DNSName": format!("{}{name}-1234567890.{r}.elb.amazonaws.com", if scheme == "internal" { "internal-" } else { "" }),
        "Type": "application", "Scheme": scheme, "State": {"Code": "active"}, "CreatedTime": ago(Duration::days(600)), "VpcId": "vpc-0demo",
    })
}

fn target_group(r: &str, s: &Svc) -> Value {
    let lb_name = if s.name == "web" { "shop-public" } else { "shop-internal" };
    let name = format!("{}-tg", s.name);
    json!({
        "TargetGroupArn": format!("arn:aws:elasticloadbalancing:{r}:{ACCT}:targetgroup/{name}/{}", hex(&name, 16)), "TargetGroupName": name,
        "Protocol": "HTTP", "Port": s.port, "TargetType": "ip", "VpcId": "vpc-0demo", "LoadBalancerArns": [lb(r, lb_name, "")["LoadBalancerArn"]],
        "HealthCheckProtocol": "HTTP", "HealthCheckPort": "traffic-port", "HealthCheckPath": "/healthz", "HealthCheckIntervalSeconds": 15,
        "HealthCheckTimeoutSeconds": 5, "HealthyThresholdCount": 2, "UnhealthyThresholdCount": 3, "Matcher": {"HttpCode": "200"},
    })
}

fn scaled() -> impl Iterator<Item = (&'static Svc, usize, usize)> {
    SVCS.iter().filter(|s| matches!(s.name, "web" | "api" | "checkout" | "payments-worker")).map(|s| (s, s.desired.div_ceil(2), s.desired * 3))
}

/// A smooth daily-looking curve with some noise, per metric and dimension set.
fn metric_value(metric: &str, stat: &str, seed: u64, i: f64) -> f64 {
    let phase = (seed % 100) as f64 / 10.0;
    let wave = ((i / 9.0 + phase).sin() + 1.0) / 2.0;
    let noise = (hash(&format!("{seed}{}", i as i64)) % 1000) as f64 / 1000.0;
    let peak = if stat == "Maximum" { 1.35 } else { 1.0 };
    let v = match metric {
        "CPUUtilization" => (22.0 + 30.0 * wave + 8.0 * noise) * peak,
        "MemoryUtilization" => (48.0 + 6.0 * wave + 3.0 * noise) * peak,
        "RequestCount" => 18_000.0 + 14_000.0 * wave + 2_000.0 * noise,
        "RequestCountPerTarget" => 2_200.0 + 1_600.0 * wave + 300.0 * noise,
        "HTTPCode_ELB_5XX_Count" | "HTTPCode_Target_5XX_Count" => if noise > 0.93 { (noise * 40.0).round() } else { (noise * 3.0).round() },
        "TargetResponseTime" => 0.09 + 0.07 * wave + 0.05 * noise,
        "HealthyHostCount" => 8.0,
        "UnHealthyHostCount" => 0.0,
        // stripe-webhook is the one failing (its dimension hash is fixed, so pick by the seed of its name).
        "Errors" => if seed == hash(&json!([{"Name": "FunctionName", "Value": "stripe-webhook"}]).to_string()) { (noise * 12.0).round() } else { 0.0 },
        "Throttles" => 0.0,
        "Invocations" => 400.0 + 300.0 * wave,
        "Duration" => 80.0 + 60.0 * wave,
        "FreeStorageSpace" => 6e9 + 2e9 * wave,
        "FreeableMemory" => 2.1e9 + 3e8 * wave,
        "DatabaseConnections" => (40.0 + 25.0 * wave).round(),
        "ReadLatency" | "WriteLatency" => 0.0015 + 0.001 * noise,
        "ApproximateAgeOfOldestMessage" => if seed % 5 == 0 { 300.0 + 600.0 * wave } else { 2.0 + 20.0 * noise },
        _ => 10.0 * wave,
    };
    let v = if metric.ends_with("Utilization") { v.min(100.0) } else { v };
    (v * 1000.0).round() / 1000.0
}

/// Lines of each stream between `start` and `end`: one roughly every 1.5 s per stream, the same
/// lines on every call (event ids follow the time), so the live tail keeps adding new ones.
fn events(streams: &[String], start: i64, end: i64, pattern: Option<&str>, max: usize) -> (Vec<Value>, bool) {
    const STEP: i64 = 1_500;
    let mut ev = vec![];
    for st in streams {
        let parts: Vec<&str> = st.split('/').collect();
        let container = parts.get(1).copied().unwrap_or("app");
        let seed = hash(st);
        let mut slot = start / STEP;
        while slot * STEP <= end {
            let h = hash(&format!("{seed}{slot}"));
            let ts = slot * STEP + (h % STEP as u64) as i64;
            slot += 1;
            if ts < start || ts > end || h % 7 == 0 {
                continue;
            }
            let msg = line(container, h);
            if pattern.is_some_and(|p| !p.split_whitespace().all(|w| msg.contains(w.trim_matches('"')))) {
                continue;
            }
            ev.push(json!({"logStreamName": st, "timestamp": ts, "ingestionTime": ts + 400, "message": msg, "eventId": format!("{seed}{ts}")}));
        }
    }
    ev.sort_by_key(|e| e["timestamp"].as_i64());
    let cut = ev.len() > max;
    ev.truncate(max);
    (ev, cut)
}

fn line(container: &str, h: u64) -> String {
    let pick = |xs: &[&str]| xs[(h / 7) as usize % xs.len()].to_string();
    let ms = 3 + h % 180;
    let id = format!("{:06}", h % 1_000_000);
    let r = h % 100;
    match container {
        "otel-collector" => format!("info  exporter sent {} spans, {} metric points", 200 + h % 900, 40 + h % 90),
        "web" => match r {
            0..3 => format!("WARN  upstream api slow: GET /v1/products took {}ms", 800 + h % 900),
            _ => format!("INFO  {} {} {}ms", pick(&["GET / 200", "GET /products 200", "GET /cart 200", "GET /static/app.js 304", "GET /search?q=shoes 200", "POST /cart/items 201"]), "-", ms),
        },
        "api" => match r {
            0..2 => format!("ERROR request failed: GET /v1/orders/{id} 500: connection reset by peer (db pool)"),
            2..6 => format!("WARN  slow query {}ms: SELECT * FROM orders WHERE customer_id = $1", 300 + h % 700),
            _ => format!("INFO  {} {ms}ms", pick(&["GET /v1/products 200", "GET /v1/products/{id} 200", "GET /v1/cart 200", "POST /v1/cart/items 201", "GET /v1/orders 200", "GET /healthz 200"]).replace("{id}", &id)),
        },
        "checkout" => match r {
            0..4 => format!("WARN  payment declined: order {id} card_declined"),
            _ => format!("INFO  {}", pick(&["order {id} created", "order {id} paid in {ms}ms", "order {id} confirmation queued", "tax computed for order {id}"]).replace("{id}", &id).replace("{ms}", &ms.to_string())),
        },
        "image-resizer" => match r {
            0..10 => format!("ERROR heap {}MB of 512MB: resizing original_{id}.png 8000x6000", 420 + h % 90),
            _ => format!("INFO  resized photo_{id}.jpg to 3 sizes in {ms}ms"),
        },
        "payments-worker" => format!("INFO  {}", pick(&["settled payout batch {id}", "refund {id} processed", "webhook evt_{id} handled", "reconciled 120 charges"]).replace("{id}", &id)),
        "search-indexer" => format!("INFO  indexed {} documents into products-v7 in {ms}ms", 50 + h % 400),
        "email-sender" => format!("INFO  {}", pick(&["sent order-confirmation to customer {id}", "sent password-reset to customer {id}", "bounce recorded for customer {id}"]).replace("{id}", &id)),
        _ => format!("INFO  job tick {id} ok in {ms}ms"),
    }
}
