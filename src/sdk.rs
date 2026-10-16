//! The transport: AWS SDK for Rust clients, one shared runtime, credentials from ~/.aws like the CLI.
//!
//! Views were written against the AWS CLI's JSON, and still read it: `run` takes the CLI's
//! service, operation and arguments (also what `H` shows) and returns the same JSON shape,
//! with the CLI's field names, pagination and `--max-items` handling. Only the reads listed in
//! `dispatch` exist here; anything else is refused, behind the guard in `aws.rs`.

use crate::aws::{classify, AwsError, Ctx};
use crate::logic::REDACTED;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, OnceLock};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime"))
}

static CONFIGS: Mutex<Option<HashMap<String, SdkConfig>>> = Mutex::new(None);

/// One loaded config per profile, so credentials are fetched once and shared by every region
/// (the region scan asks 17 regions at once); each region gets a copy with the same credential cache.
async fn config(ctx: &Ctx) -> SdkConfig {
    let cached = CONFIGS.lock().unwrap().get_or_insert_with(HashMap::new).get(&ctx.profile).cloned();
    let base = match cached {
        Some(c) => c,
        None => {
            let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(ctx.region.clone()));
            // No profile means the default chain: AWS_PROFILE, env credentials, [default], instance role.
            if !ctx.profile.is_empty() {
                loader = loader.profile_name(&ctx.profile);
            }
            let c = loader.load().await;
            CONFIGS.lock().unwrap().get_or_insert_with(HashMap::new).entry(ctx.profile.clone()).or_insert(c).clone()
        }
    };
    base.to_builder().region(Region::new(ctx.region.clone())).build()
}

/// Forget loaded credentials, after `aws sso login` or an expired session, so the next call reads them again.
pub fn reset() {
    *CONFIGS.lock().unwrap() = None;
}

/// One read, by the CLI's names: `run(ctx, "ecs", "list-tasks", ["--cluster", "prod"])`.
pub fn run(ctx: &Ctx, service: &str, op: &str, args: &[String]) -> Result<Value, AwsError> {
    let mut v = runtime().block_on(dispatch(ctx, service, op, &Args(args)))?;
    prune(&mut v);
    Ok(v)
}

/// The CLI arguments the views build, read back.
struct Args<'a>(&'a [String]);

impl Args<'_> {
    fn all(&self, flag: &str) -> Vec<String> {
        self.0.iter().skip_while(|a| *a != flag).skip(1).take_while(|a| !a.starts_with("--")).cloned().collect()
    }
    fn one(&self, flag: &str) -> Option<String> {
        self.all(flag).into_iter().next()
    }
    /// The argument right after `flag`, even one starting with "--" (a secret value can).
    fn raw(&self, flag: &str) -> Option<String> {
        self.0.iter().skip_while(|a| *a != flag).nth(1).cloned()
    }
    fn has(&self, flag: &str) -> bool {
        self.0.iter().any(|a| a == flag)
    }
    fn max_items(&self) -> Option<usize> {
        self.one("--max-items").and_then(|v| v.parse().ok())
    }
}

/// An SDK error as one readable line (each cause once, no debug dump), through the same `classify`
/// the CLI's stderr went through, so an expired SSO session still shows as such.
fn fail<E: std::error::Error + 'static>(ctx: &Ctx, service: &str, op: &str) -> impl Fn(E) -> AwsError {
    let (profile, service, op) = (ctx.profile.clone(), service.to_string(), op.to_string());
    move |e| classify(&message(&e), &profile, &service, &op)
}

fn message(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = vec![];
    let mut cur = Some(e);
    while let Some(x) = cur {
        let t = x.to_string();
        if !parts.iter().any(|p| p.contains(&t)) && !matches!(t.as_str(), "service error" | "dispatch failure" | "other") {
            parts.push(t);
        }
        cur = x.source();
    }
    parts.join(": ")
}

/// Pages until there is no token or `max` items are in: the CLI's auto-pagination and `--max-items`.
/// Returns the items and whether more exist (the CLI then prints a NextToken).
async fn pages<F, Fut>(max: Option<usize>, mut page: F) -> Result<(Vec<Value>, bool), AwsError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<Value>, Option<String>), AwsError>>,
{
    let (mut items, mut token) = (vec![], None);
    loop {
        let (got, next) = page(token).await?;
        items.extend(got);
        token = next;
        if token.is_none() || max.is_some_and(|m| items.len() >= m) {
            break;
        }
    }
    let more = token.is_some() || max.is_some_and(|m| items.len() > m);
    if let Some(m) = max {
        items.truncate(m);
    }
    Ok((items, more))
}

/// `{key: items}`, with the CLI's NextToken when the list was cut at `--max-items`.
fn listed(key: &str, (items, more): (Vec<Value>, bool)) -> Value {
    let mut v = json!({ key: items });
    if more {
        v["NextToken"] = json!("more");
    }
    v
}

/// RFC 3339 to the millisecond: the SDK keeps times as float seconds, which print as .476999.
fn ts(d: Option<&aws_smithy_types::DateTime>) -> Value {
    d.and_then(|d| chrono::DateTime::from_timestamp_millis(d.secs() * 1000 + (i64::from(d.subsec_nanos()) + 500_000) / 1_000_000))
        .map_or(Value::Null, |t| Value::String(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
}

fn secs(s: &str) -> aws_smithy_types::DateTime {
    let t = chrono::DateTime::parse_from_rfc3339(s).map(|t| t.timestamp()).unwrap_or(0);
    aws_smithy_types::DateTime::from_secs(t)
}

/// Absent fields are left out, as the CLI does (a view tests `get("healthCheck").is_some()`).
fn prune(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, x| !x.is_null());
            m.values_mut().for_each(prune);
        }
        Value::Array(a) => a.iter_mut().for_each(prune),
        _ => {}
    }
}

async fn dispatch(ctx: &Ctx, service: &str, op: &str, a: &Args<'_>) -> Result<Value, AwsError> {
    let cfg = config(ctx).await;
    let max = a.max_items();
    macro_rules! e {
        () => {
            fail(ctx, service, op)
        };
    }
    Ok(match (service, op) {
        ("sts", "get-caller-identity") => {
            let r = aws_sdk_sts::Client::new(&cfg).get_caller_identity().send().await.map_err(e!())?;
            json!({"Account": r.account(), "Arn": r.arn(), "UserId": r.user_id()})
        }

        // ---------- EC2 ----------
        ("ec2", "describe-regions") => {
            let r = aws_sdk_ec2::Client::new(&cfg).describe_regions().send().await.map_err(e!())?;
            json!({"Regions": r.regions().iter().map(|x| json!({"RegionName": x.region_name()})).collect::<Vec<_>>()})
        }
        ("ec2", "describe-instances") => {
            let c = aws_sdk_ec2::Client::new(&cfg);
            listed("Reservations", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.describe_instances().set_next_token(t).send().await.map_err(e!())?;
                    let res = r.reservations().iter().map(|rv| json!({"Instances": rv.instances().iter().map(ec2_instance).collect::<Vec<_>>()})).collect();
                    Ok((res, r.next_token().map(String::from)))
                }
            }).await?)
        }

        // ---------- ECS ----------
        ("ecs", "list-clusters") => {
            let c = aws_sdk_ecs::Client::new(&cfg);
            listed("clusterArns", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.list_clusters().set_next_token(t).send().await.map_err(e!())?;
                    Ok((r.cluster_arns().iter().map(|x| json!(x)).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("ecs", "describe-clusters") => {
            let mut q = aws_sdk_ecs::Client::new(&cfg).describe_clusters().set_clusters(Some(a.all("--clusters")));
            for f in a.all("--include") {
                q = q.include(f.as_str().into());
            }
            let r = q.send().await.map_err(e!())?;
            json!({"clusters": r.clusters().iter().map(ecs_cluster).collect::<Vec<_>>(), "failures": ecs_failures(r.failures())})
        }
        ("ecs", "list-services") => {
            let c = aws_sdk_ecs::Client::new(&cfg);
            let cluster = a.one("--cluster");
            listed("serviceArns", pages(max, |t| {
                let (c, cluster) = (c.clone(), cluster.clone());
                async move {
                    let r = c.list_services().set_cluster(cluster).set_next_token(t).send().await.map_err(e!())?;
                    Ok((r.service_arns().iter().map(|x| json!(x)).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("ecs", "describe-services") => {
            let r = aws_sdk_ecs::Client::new(&cfg)
                .describe_services()
                .set_cluster(a.one("--cluster"))
                .set_services(Some(a.all("--services")))
                .send()
                .await
                .map_err(e!())?;
            json!({"services": r.services().iter().map(ecs_service).collect::<Vec<_>>(), "failures": ecs_failures(r.failures())})
        }
        // --write only (aws.rs), and only the desired count or a restart: no other update-service argument is sent.
        ("ecs", "update-service") => {
            let r = aws_sdk_ecs::Client::new(&cfg)
                .update_service()
                .set_cluster(a.one("--cluster"))
                .set_service(a.one("--service"))
                .set_desired_count(a.one("--desired-count").and_then(|v| v.parse().ok()))
                .set_force_new_deployment(a.has("--force-new-deployment").then_some(true))
                .send()
                .await
                .map_err(e!())?;
            json!({"service": r.service().map(ecs_service)})
        }
        ("ecs", "list-tasks") => {
            let c = aws_sdk_ecs::Client::new(&cfg);
            let (cluster, status, svc) = (a.one("--cluster"), a.one("--desired-status"), a.one("--service-name"));
            listed("taskArns", pages(max, |t| {
                let (c, cluster, status, svc) = (c.clone(), cluster.clone(), status.clone(), svc.clone());
                async move {
                    let r = c
                        .list_tasks()
                        .set_cluster(cluster)
                        .set_desired_status(status.map(|s| s.as_str().into()))
                        .set_service_name(svc)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    Ok((r.task_arns().iter().map(|x| json!(x)).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("ecs", "describe-tasks") => {
            let r = aws_sdk_ecs::Client::new(&cfg)
                .describe_tasks()
                .set_cluster(a.one("--cluster"))
                .set_tasks(Some(a.all("--tasks")))
                .send()
                .await
                .map_err(e!())?;
            json!({"tasks": r.tasks().iter().map(ecs_task).collect::<Vec<_>>(), "failures": ecs_failures(r.failures())})
        }
        ("ecs", "list-task-definition-families") => {
            let c = aws_sdk_ecs::Client::new(&cfg);
            let status = a.one("--status");
            listed("families", pages(max, |t| {
                let (c, status) = (c.clone(), status.clone());
                async move {
                    let r = c.list_task_definition_families().set_status(status.map(|s| s.as_str().into())).set_next_token(t).send().await.map_err(e!())?;
                    Ok((r.families().iter().map(|x| json!(x)).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("ecs", "list-task-definitions") => {
            let c = aws_sdk_ecs::Client::new(&cfg);
            let (family, status, sort) = (a.one("--family-prefix"), a.one("--status"), a.one("--sort"));
            listed("taskDefinitionArns", pages(max, |t| {
                let (c, family, status, sort) = (c.clone(), family.clone(), status.clone(), sort.clone());
                async move {
                    let r = c
                        .list_task_definitions()
                        .set_family_prefix(family)
                        .set_status(status.map(|s| s.as_str().into()))
                        .set_sort(sort.map(|s| s.as_str().into()))
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    Ok((r.task_definition_arns().iter().map(|x| json!(x)).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("ecs", "describe-task-definition") => {
            let r = aws_sdk_ecs::Client::new(&cfg).describe_task_definition().set_task_definition(a.one("--task-definition")).send().await.map_err(e!())?;
            json!({"taskDefinition": r.task_definition().map(ecs_taskdef)})
        }

        // ---------- CloudWatch Logs ----------
        ("logs", "describe-log-groups") => {
            let c = aws_sdk_cloudwatchlogs::Client::new(&cfg);
            listed("logGroups", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.describe_log_groups().set_next_token(t).send().await.map_err(e!())?;
                    let g = r
                        .log_groups()
                        .iter()
                        .map(|g| json!({
                            "logGroupName": g.log_group_name(), "retentionInDays": g.retention_in_days(),
                            "storedBytes": g.stored_bytes(), "creationTime": g.creation_time(), "arn": g.arn(),
                        }))
                        .collect();
                    Ok((g, r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("logs", "describe-log-streams") => {
            let c = aws_sdk_cloudwatchlogs::Client::new(&cfg);
            let (group, order) = (a.one("--log-group-name"), a.one("--order-by"));
            let desc = a.has("--descending");
            listed("logStreams", pages(max, |t| {
                let (c, group, order) = (c.clone(), group.clone(), order.clone());
                async move {
                    let r = c
                        .describe_log_streams()
                        .set_log_group_name(group)
                        .set_order_by(order.map(|o| o.as_str().into()))
                        .descending(desc)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let s = r
                        .log_streams()
                        .iter()
                        .map(|s| json!({
                            "logStreamName": s.log_stream_name(), "creationTime": s.creation_time(),
                            "firstEventTimestamp": s.first_event_timestamp(), "lastEventTimestamp": s.last_event_timestamp(),
                        }))
                        .collect();
                    Ok((s, r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("logs", "filter-log-events") => {
            let c = aws_sdk_cloudwatchlogs::Client::new(&cfg);
            let (group, prefix, pattern) = (a.one("--log-group-name"), a.one("--log-stream-name-prefix"), a.one("--filter-pattern"));
            let streams = Some(a.all("--log-stream-names")).filter(|s| !s.is_empty());
            let start = a.one("--start-time").and_then(|s| s.parse::<i64>().ok());
            listed("events", pages(max, |t| {
                let (c, group, prefix, pattern, streams) = (c.clone(), group.clone(), prefix.clone(), pattern.clone(), streams.clone());
                async move {
                    let r = c
                        .filter_log_events()
                        .set_log_group_name(group)
                        .set_log_stream_names(streams)
                        .set_log_stream_name_prefix(prefix)
                        .set_filter_pattern(pattern)
                        .set_start_time(start)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let ev = r
                        .events()
                        .iter()
                        .map(|x| json!({
                            "logStreamName": x.log_stream_name(), "timestamp": x.timestamp(), "message": x.message(),
                            "ingestionTime": x.ingestion_time(), "eventId": x.event_id(),
                        }))
                        .collect();
                    Ok((ev, r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("logs", "get-log-events") => {
            let r = aws_sdk_cloudwatchlogs::Client::new(&cfg)
                .get_log_events()
                .set_log_group_name(a.one("--log-group-name"))
                .set_log_stream_name(a.one("--log-stream-name"))
                .start_from_head(a.has("--start-from-head"))
                .set_limit(a.one("--limit").and_then(|l| l.parse().ok()))
                .send()
                .await
                .map_err(e!())?;
            let ev: Vec<Value> =
                r.events().iter().map(|x| json!({"timestamp": x.timestamp(), "message": x.message(), "ingestionTime": x.ingestion_time()})).collect();
            json!({"events": ev})
        }

        // ---------- ELBv2 ----------
        ("elbv2", "describe-load-balancers") => {
            let c = aws_sdk_elasticloadbalancingv2::Client::new(&cfg);
            listed("LoadBalancers", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.describe_load_balancers().set_marker(t).send().await.map_err(e!())?;
                    let lbs = r
                        .load_balancers()
                        .iter()
                        .map(|l| json!({
                            "LoadBalancerArn": l.load_balancer_arn(), "LoadBalancerName": l.load_balancer_name(), "DNSName": l.dns_name(),
                            "Type": l.r#type().map(|x| x.as_str()), "Scheme": l.scheme().map(|x| x.as_str()),
                            "State": {"Code": l.state().and_then(|s| s.code()).map(|x| x.as_str())},
                            "CreatedTime": ts(l.created_time()), "VpcId": l.vpc_id(),
                        }))
                        .collect();
                    Ok((lbs, r.next_marker().map(String::from)))
                }
            }).await?)
        }
        ("elbv2", "describe-target-groups") => {
            let c = aws_sdk_elasticloadbalancingv2::Client::new(&cfg);
            let lb = a.one("--load-balancer-arn");
            listed("TargetGroups", pages(max, |t| {
                let (c, lb) = (c.clone(), lb.clone());
                async move {
                    let r = c.describe_target_groups().set_load_balancer_arn(lb).set_marker(t).send().await.map_err(e!())?;
                    Ok((r.target_groups().iter().map(target_group).collect(), r.next_marker().map(String::from)))
                }
            }).await?)
        }
        ("elbv2", "describe-target-health") => {
            let r = aws_sdk_elasticloadbalancingv2::Client::new(&cfg)
                .describe_target_health()
                .set_target_group_arn(a.one("--target-group-arn"))
                .send()
                .await
                .map_err(e!())?;
            let d: Vec<Value> = r
                .target_health_descriptions()
                .iter()
                .map(|d| json!({
                    "Target": d.target().map(|t| json!({"Id": t.id(), "Port": t.port(), "AvailabilityZone": t.availability_zone()})),
                    "HealthCheckPort": d.health_check_port(),
                    "TargetHealth": d.target_health().map(|h| json!({
                        "State": h.state().map(|x| x.as_str()), "Reason": h.reason().map(|x| x.as_str()), "Description": h.description(),
                    })),
                }))
                .collect();
            json!({"TargetHealthDescriptions": d})
        }
        ("elbv2", "describe-target-group-attributes") => {
            let r = aws_sdk_elasticloadbalancingv2::Client::new(&cfg)
                .describe_target_group_attributes()
                .set_target_group_arn(a.one("--target-group-arn"))
                .send()
                .await
                .map_err(e!())?;
            json!({"Attributes": r.attributes().iter().map(|x| json!({"Key": x.key(), "Value": x.value()})).collect::<Vec<_>>()})
        }

        // ---------- Application Auto Scaling ----------
        ("application-autoscaling", "describe-scalable-targets") => {
            let c = aws_sdk_applicationautoscaling::Client::new(&cfg);
            let (ns, ids) = (a.one("--service-namespace"), Some(a.all("--resource-ids")).filter(|x| !x.is_empty()));
            listed("ScalableTargets", pages(max, |t| {
                let (c, ns, ids) = (c.clone(), ns.clone(), ids.clone());
                async move {
                    let r = c
                        .describe_scalable_targets()
                        .set_service_namespace(ns.map(|n| n.as_str().into()))
                        .set_resource_ids(ids)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let x = r
                        .scalable_targets()
                        .iter()
                        .map(|x| json!({
                            "ServiceNamespace": x.service_namespace().as_str(), "ResourceId": x.resource_id(),
                            "ScalableDimension": x.scalable_dimension().as_str(), "MinCapacity": x.min_capacity(), "MaxCapacity": x.max_capacity(),
                            "CreationTime": ts(Some(x.creation_time())),
                            "SuspendedState": x.suspended_state().map(|s| json!({
                                "DynamicScalingInSuspended": s.dynamic_scaling_in_suspended(),
                                "DynamicScalingOutSuspended": s.dynamic_scaling_out_suspended(),
                                "ScheduledScalingSuspended": s.scheduled_scaling_suspended(),
                            })),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("application-autoscaling", "describe-scheduled-actions") => {
            let c = aws_sdk_applicationautoscaling::Client::new(&cfg);
            let ns = a.one("--service-namespace");
            listed("ScheduledActions", pages(max, |t| {
                let (c, ns) = (c.clone(), ns.clone());
                async move {
                    let r = c.describe_scheduled_actions().set_service_namespace(ns.map(|n| n.as_str().into())).set_next_token(t).send().await.map_err(e!())?;
                    let x = r
                        .scheduled_actions()
                        .iter()
                        .map(|x| json!({
                            "ScheduledActionName": x.scheduled_action_name(), "ResourceId": x.resource_id(),
                            "ScalableDimension": x.scalable_dimension().map(|d| d.as_str()), "Schedule": x.schedule(),
                            "ScalableTargetAction": x.scalable_target_action().map(|a| json!({"MinCapacity": a.min_capacity(), "MaxCapacity": a.max_capacity()})),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }
        // --write only (aws.rs).
        ("application-autoscaling", "register-scalable-target") => {
            aws_sdk_applicationautoscaling::Client::new(&cfg)
                .register_scalable_target()
                .set_service_namespace(a.one("--service-namespace").map(|n| n.as_str().into()))
                .set_resource_id(a.one("--resource-id"))
                .set_scalable_dimension(a.one("--scalable-dimension").map(|d| d.as_str().into()))
                .set_min_capacity(a.one("--min-capacity").and_then(|v| v.parse().ok()))
                .set_max_capacity(a.one("--max-capacity").and_then(|v| v.parse().ok()))
                .send()
                .await
                .map_err(e!())?;
            json!({})
        }
        // --write only. `--scalable-target-action MinCapacity=2,MaxCapacity=10`, the CLI's shorthand.
        ("application-autoscaling", "put-scheduled-action") => {
            let act: HashMap<String, i32> = a
                .one("--scalable-target-action")
                .unwrap_or_default()
                .split(',')
                .filter_map(|kv| kv.split_once('=').and_then(|(k, v)| Some((k.to_string(), v.parse().ok()?))))
                .collect();
            aws_sdk_applicationautoscaling::Client::new(&cfg)
                .put_scheduled_action()
                .set_service_namespace(a.one("--service-namespace").map(|n| n.as_str().into()))
                .set_scheduled_action_name(a.one("--scheduled-action-name"))
                .set_resource_id(a.one("--resource-id"))
                .set_scalable_dimension(a.one("--scalable-dimension").map(|d| d.as_str().into()))
                .set_schedule(a.one("--schedule"))
                .scalable_target_action(
                    aws_sdk_applicationautoscaling::types::ScalableTargetAction::builder()
                        .set_min_capacity(act.get("MinCapacity").copied())
                        .set_max_capacity(act.get("MaxCapacity").copied())
                        .build(),
                )
                .send()
                .await
                .map_err(e!())?;
            json!({})
        }
        ("application-autoscaling", "describe-scaling-policies") => {
            let c = aws_sdk_applicationautoscaling::Client::new(&cfg);
            let (ns, res) = (a.one("--service-namespace"), a.one("--resource-id"));
            listed("ScalingPolicies", pages(max, |t| {
                let (c, ns, res) = (c.clone(), ns.clone(), res.clone());
                async move {
                    let r = c
                        .describe_scaling_policies()
                        .set_service_namespace(ns.map(|n| n.as_str().into()))
                        .set_resource_id(res)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    Ok((r.scaling_policies().iter().map(scaling_policy).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("application-autoscaling", "describe-scaling-activities") => {
            let c = aws_sdk_applicationautoscaling::Client::new(&cfg);
            let (ns, res) = (a.one("--service-namespace"), a.one("--resource-id"));
            listed("ScalingActivities", pages(max, |t| {
                let (c, ns, res) = (c.clone(), ns.clone(), res.clone());
                async move {
                    let r = c
                        .describe_scaling_activities()
                        .set_service_namespace(ns.map(|n| n.as_str().into()))
                        .set_resource_id(res)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let x = r
                        .scaling_activities()
                        .iter()
                        .map(|x| json!({
                            "ActivityId": x.activity_id(), "ResourceId": x.resource_id(), "Description": x.description(), "Cause": x.cause(),
                            "StatusCode": x.status_code().as_str(), "StatusMessage": x.status_message(), "StartTime": ts(Some(x.start_time())),
                            "EndTime": ts(x.end_time()),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }

        // ---------- CloudWatch metrics ----------
        ("cloudwatch", "get-metric-data") => {
            use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat, ScanBy};
            let queries: Vec<Value> = a.one("--metric-data-queries").and_then(|q| serde_json::from_str(&q).ok()).unwrap_or_default();
            let q: Vec<MetricDataQuery> = queries
                .iter()
                .map(|q| {
                    let m = &q["MetricStat"];
                    let dims = m["Metric"]["Dimensions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|d| Dimension::builder().name(d["Name"].as_str().unwrap_or_default()).value(d["Value"].as_str().unwrap_or_default()).build())
                        .collect();
                    let metric = Metric::builder()
                        .namespace(m["Metric"]["Namespace"].as_str().unwrap_or_default())
                        .metric_name(m["Metric"]["MetricName"].as_str().unwrap_or_default())
                        .set_dimensions(Some(dims))
                        .build();
                    let stat = MetricStat::builder().metric(metric).period(m["Period"].as_i64().unwrap_or(300) as i32).stat(m["Stat"].as_str().unwrap_or_default()).build();
                    MetricDataQuery::builder()
                        .id(q["Id"].as_str().unwrap_or_default())
                        .label(q["Label"].as_str().unwrap_or_default())
                        .metric_stat(stat)
                        .return_data(true)
                        .build()
                })
                .collect();
            let c = aws_sdk_cloudwatch::Client::new(&cfg);
            let (start, end) = (secs(&a.one("--start-time").unwrap_or_default()), secs(&a.one("--end-time").unwrap_or_default()));
            listed("MetricDataResults", pages(max, |t| {
                let (c, q) = (c.clone(), q.clone());
                async move {
                    let r = c
                        .get_metric_data()
                        .set_metric_data_queries(Some(q))
                        .start_time(start)
                        .end_time(end)
                        .scan_by(ScanBy::TimestampAscending)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let x = r
                        .metric_data_results()
                        .iter()
                        .map(|x| json!({
                            "Id": x.id(), "Label": x.label(), "StatusCode": x.status_code().map(|s| s.as_str()),
                            "Timestamps": x.timestamps().iter().map(|t| ts(Some(t))).collect::<Vec<_>>(), "Values": x.values(),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }

        // ---------- ECR ----------
        ("ecr", "describe-repositories") => {
            let c = aws_sdk_ecr::Client::new(&cfg);
            listed("repositories", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.describe_repositories().set_next_token(t).send().await.map_err(e!())?;
                    let x = r
                        .repositories()
                        .iter()
                        .map(|x| json!({
                            "repositoryName": x.repository_name(), "repositoryArn": x.repository_arn(), "repositoryUri": x.repository_uri(),
                            "imageTagMutability": x.image_tag_mutability().map(|m| m.as_str()), "createdAt": ts(x.created_at()),
                            "imageScanningConfiguration": x.image_scanning_configuration().map(|s| json!({"scanOnPush": s.scan_on_push()})),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("ecr", "describe-images") => {
            let c = aws_sdk_ecr::Client::new(&cfg);
            let repo = a.one("--repository-name");
            listed("imageDetails", pages(max, |t| {
                let (c, repo) = (c.clone(), repo.clone());
                async move {
                    let r = c.describe_images().set_repository_name(repo).set_next_token(t).send().await.map_err(e!())?;
                    let x = r
                        .image_details()
                        .iter()
                        .map(|x| json!({
                            "imageDigest": x.image_digest(), "imageTags": x.image_tags(), "imageSizeInBytes": x.image_size_in_bytes(),
                            "imagePushedAt": ts(x.image_pushed_at()), "lastRecordedPullTime": ts(x.last_recorded_pull_time()),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }

        // ---------- SQS ----------
        ("sqs", "list-queues") => {
            let c = aws_sdk_sqs::Client::new(&cfg);
            listed("QueueUrls", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.list_queues().set_next_token(t).send().await.map_err(e!())?;
                    Ok((r.queue_urls().iter().map(|x| json!(x)).collect(), r.next_token().map(String::from)))
                }
            }).await?)
        }
        ("sqs", "get-queue-attributes") => {
            let r = aws_sdk_sqs::Client::new(&cfg)
                .get_queue_attributes()
                .set_queue_url(a.one("--queue-url"))
                .attribute_names(aws_sdk_sqs::types::QueueAttributeName::All)
                .send()
                .await
                .map_err(e!())?;
            let at: serde_json::Map<String, Value> = r.attributes().into_iter().flatten().map(|(k, v)| (k.as_str().to_string(), json!(v))).collect();
            json!({"Attributes": at})
        }

        // ---------- RDS ----------
        ("rds", "describe-db-clusters") => {
            let c = aws_sdk_rds::Client::new(&cfg);
            let id = a.one("--db-cluster-identifier");
            listed("DBClusters", pages(max, |t| {
                let (c, id) = (c.clone(), id.clone());
                async move {
                    let r = c.describe_db_clusters().set_db_cluster_identifier(id).set_marker(t).send().await.map_err(e!())?;
                    let x = r
                        .db_clusters()
                        .iter()
                        .map(|x| json!({
                            "DBClusterIdentifier": x.db_cluster_identifier(), "DBClusterArn": x.db_cluster_arn(), "Engine": x.engine(),
                            "EngineVersion": x.engine_version(), "DBClusterInstanceClass": x.db_cluster_instance_class(), "Status": x.status(),
                            "MultiAZ": x.multi_az(), "ClusterCreateTime": ts(x.cluster_create_time()), "Endpoint": x.endpoint(),
                            "ReaderEndpoint": x.reader_endpoint(), "PreferredMaintenanceWindow": x.preferred_maintenance_window(),
                            "BackupRetentionPeriod": x.backup_retention_period(), "DeletionProtection": x.deletion_protection(),
                            "PendingModifiedValues": x.pending_modified_values().map(|p| json!({
                                "DBClusterIdentifier": p.db_cluster_identifier(), "EngineVersion": p.engine_version(),
                                "BackupRetentionPeriod": p.backup_retention_period(), "AllocatedStorage": p.allocated_storage(),
                                "Iops": p.iops(), "StorageType": p.storage_type(), "CertificateDetails": p.certificate_details().and_then(|c| c.ca_identifier()),
                                "IAMDatabaseAuthenticationEnabled": p.iam_database_authentication_enabled(),
                                "MasterUserPassword": p.master_user_password().map(|_| "(changing)"),
                            })),
                            "DBClusterMembers": x.db_cluster_members().iter().map(|m| json!({
                                "DBInstanceIdentifier": m.db_instance_identifier(), "IsClusterWriter": m.is_cluster_writer(),
                            })).collect::<Vec<_>>(),
                        }))
                        .collect();
                    Ok((x, r.marker().map(String::from)))
                }
            }).await?)
        }
        ("rds", "describe-db-instances") => {
            let c = aws_sdk_rds::Client::new(&cfg);
            let id = a.one("--db-instance-identifier");
            listed("DBInstances", pages(max, |t| {
                let (c, id) = (c.clone(), id.clone());
                async move {
                    let r = c.describe_db_instances().set_db_instance_identifier(id).set_marker(t).send().await.map_err(e!())?;
                    let x = r
                        .db_instances()
                        .iter()
                        .map(|x| json!({
                            "DBInstanceIdentifier": x.db_instance_identifier(), "DBInstanceArn": x.db_instance_arn(), "Engine": x.engine(),
                            "EngineVersion": x.engine_version(), "DBInstanceClass": x.db_instance_class(), "DBInstanceStatus": x.db_instance_status(),
                            "AvailabilityZone": x.availability_zone(), "InstanceCreateTime": ts(x.instance_create_time()),
                            "DBClusterIdentifier": x.db_cluster_identifier(),
                            "ReadReplicaSourceDBInstanceIdentifier": x.read_replica_source_db_instance_identifier(), "MultiAZ": x.multi_az(),
                            "AllocatedStorage": x.allocated_storage(), "MaxAllocatedStorage": x.max_allocated_storage(), "StorageType": x.storage_type(),
                            "Iops": x.iops(), "StorageThroughput": x.storage_throughput(), "StorageEncrypted": x.storage_encrypted(),
                            "Endpoint": x.endpoint().map(|e| json!({"Address": e.address(), "Port": e.port()})),
                            "PreferredMaintenanceWindow": x.preferred_maintenance_window(), "PreferredBackupWindow": x.preferred_backup_window(),
                            "BackupRetentionPeriod": x.backup_retention_period(), "LatestRestorableTime": ts(x.latest_restorable_time()),
                            "AutoMinorVersionUpgrade": x.auto_minor_version_upgrade(), "DeletionProtection": x.deletion_protection(),
                            "PubliclyAccessible": x.publicly_accessible(), "PerformanceInsightsEnabled": x.performance_insights_enabled(),
                            "CACertificateIdentifier": x.ca_certificate_identifier(),
                            "PendingModifiedValues": x.pending_modified_values().map(|p| json!({
                                "DBInstanceClass": p.db_instance_class(), "AllocatedStorage": p.allocated_storage(), "Port": p.port(),
                                "BackupRetentionPeriod": p.backup_retention_period(), "MultiAZ": p.multi_az(), "Engine": p.engine(),
                                "EngineVersion": p.engine_version(), "LicenseModel": p.license_model(), "Iops": p.iops(),
                                "StorageThroughput": p.storage_throughput(), "DBInstanceIdentifier": p.db_instance_identifier(),
                                "StorageType": p.storage_type(), "CACertificateIdentifier": p.ca_certificate_identifier(),
                                "DBSubnetGroupName": p.db_subnet_group_name(), "IAMDatabaseAuthenticationEnabled": p.iam_database_authentication_enabled(),
                                "DedicatedLogVolume": p.dedicated_log_volume(), "MultiTenant": p.multi_tenant(),
                                // Only that a new password is queued: the value is never shown (AWS masks it anyway).
                                "MasterUserPassword": p.master_user_password().map(|_| "(changing)"),
                            })),
                        }))
                        .collect();
                    Ok((x, r.marker().map(String::from)))
                }
            }).await?)
        }

        ("rds", "describe-events") => {
            let c = aws_sdk_rds::Client::new(&cfg);
            let (duration, kind, id) = (a.one("--duration").and_then(|d| d.parse().ok()), a.one("--source-type"), a.one("--source-identifier"));
            listed("Events", pages(max, |t| {
                let (c, kind, id) = (c.clone(), kind.clone(), id.clone());
                async move {
                    let r = c
                        .describe_events()
                        .set_duration(duration)
                        .set_source_type(kind.map(|k| aws_sdk_rds::types::SourceType::from(k.as_str())))
                        .set_source_identifier(id)
                        .set_marker(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let x = r
                        .events()
                        .iter()
                        .map(|x| json!({
                            "SourceIdentifier": x.source_identifier(), "SourceType": x.source_type().map(|k| k.as_str()),
                            "Message": x.message(), "EventCategories": x.event_categories(), "Date": ts(x.date()), "SourceArn": x.source_arn(),
                        }))
                        .collect();
                    Ok((x, r.marker().map(String::from)))
                }
            }).await?)
        }

        ("rds", "describe-pending-maintenance-actions") => {
            let c = aws_sdk_rds::Client::new(&cfg);
            let arn = a.one("--resource-identifier");
            listed("PendingMaintenanceActions", pages(max, |t| {
                let (c, arn) = (c.clone(), arn.clone());
                async move {
                    let r = c.describe_pending_maintenance_actions().set_resource_identifier(arn).set_marker(t).send().await.map_err(e!())?;
                    let x = r
                        .pending_maintenance_actions()
                        .iter()
                        .map(|x| json!({
                            "ResourceIdentifier": x.resource_identifier(),
                            "PendingMaintenanceActionDetails": x.pending_maintenance_action_details().iter().map(|d| json!({
                                "Action": d.action(), "Description": d.description(), "OptInStatus": d.opt_in_status(),
                                "AutoAppliedAfterDate": ts(d.auto_applied_after_date()), "ForcedApplyDate": ts(d.forced_apply_date()),
                                "CurrentApplyDate": ts(d.current_apply_date()),
                            })).collect::<Vec<_>>(),
                        }))
                        .collect();
                    Ok((x, r.marker().map(String::from)))
                }
            }).await?)
        }

        // ---------- CloudWatch alarms ----------
        ("cloudwatch", "describe-alarms") => {
            use aws_sdk_cloudwatch::types::AlarmType;
            let c = aws_sdk_cloudwatch::Client::new(&cfg);
            let (mut metric, mut composite, mut token) = (vec![], vec![], None);
            loop {
                let r = c
                    .describe_alarms()
                    .set_alarm_types(Some(vec![AlarmType::MetricAlarm, AlarmType::CompositeAlarm]))
                    .set_next_token(token)
                    .send()
                    .await
                    .map_err(e!())?;
                metric.extend(r.metric_alarms().iter().map(|x| json!({
                    "AlarmName": x.alarm_name(), "AlarmArn": x.alarm_arn(), "AlarmDescription": x.alarm_description(),
                    "StateValue": x.state_value().map(|v| v.as_str()), "StateReason": x.state_reason(),
                    "StateUpdatedTimestamp": ts(x.state_updated_timestamp()), "StateTransitionedTimestamp": ts(x.state_transitioned_timestamp()),
                    "MetricName": x.metric_name(), "Namespace": x.namespace(),
                    "Dimensions": x.dimensions().iter().map(|d| json!({"Name": d.name(), "Value": d.value()})).collect::<Vec<_>>(),
                    "Statistic": x.statistic().map(|v| v.as_str()), "ExtendedStatistic": x.extended_statistic(), "Period": x.period(),
                    "EvaluationPeriods": x.evaluation_periods(), "DatapointsToAlarm": x.datapoints_to_alarm(), "Threshold": x.threshold(),
                    "ComparisonOperator": x.comparison_operator().map(|v| v.as_str()), "TreatMissingData": x.treat_missing_data(),
                    "ActionsEnabled": x.actions_enabled(), "AlarmActions": x.alarm_actions(),
                    // Alarms on metric math have no single MetricName.
                    "Metrics": x.metrics().iter().filter_map(|m| m.expression().map(|e| json!({"Id": m.id(), "Expression": e}))).collect::<Vec<_>>(),
                })));
                composite.extend(r.composite_alarms().iter().map(|x| json!({
                    "AlarmName": x.alarm_name(), "AlarmArn": x.alarm_arn(), "AlarmDescription": x.alarm_description(),
                    "StateValue": x.state_value().map(|v| v.as_str()), "StateReason": x.state_reason(),
                    "StateUpdatedTimestamp": ts(x.state_updated_timestamp()), "StateTransitionedTimestamp": ts(x.state_transitioned_timestamp()),
                    "AlarmRule": x.alarm_rule(), "ActionsEnabled": x.actions_enabled(), "AlarmActions": x.alarm_actions(),
                })));
                token = r.next_token().map(String::from);
                if token.is_none() {
                    break;
                }
            }
            json!({"MetricAlarms": metric, "CompositeAlarms": composite})
        }

        // ---------- CloudTrail ----------
        ("cloudtrail", "lookup-events") => {
            use aws_sdk_cloudtrail::types::{LookupAttribute, LookupAttributeKey};
            // The CLI's form: AttributeKey=ReadOnly,AttributeValue=false
            let attrs: Vec<LookupAttribute> = a
                .all("--lookup-attributes")
                .iter()
                .filter_map(|x| {
                    let kv: HashMap<&str, &str> = x.split(',').filter_map(|p| p.split_once('=')).collect();
                    LookupAttribute::builder()
                        .attribute_key(LookupAttributeKey::from(*kv.get("AttributeKey")?))
                        .attribute_value(*kv.get("AttributeValue")?)
                        .build()
                        .ok()
                })
                .collect();
            let start = a.one("--start-time").map(|s| secs(&s));
            let c = aws_sdk_cloudtrail::Client::new(&cfg);
            listed("Events", pages(max, |t| {
                let (c, attrs) = (c.clone(), attrs.clone());
                async move {
                    let r = c
                        .lookup_events()
                        .set_lookup_attributes(Some(attrs).filter(|x| !x.is_empty()))
                        .set_start_time(start)
                        .max_results(50)
                        .set_next_token(t)
                        .send()
                        .await
                        .map_err(e!())?;
                    let x = r
                        .events()
                        .iter()
                        .map(|x| json!({
                            "EventId": x.event_id(), "EventName": x.event_name(), "ReadOnly": x.read_only(), "EventTime": ts(x.event_time()),
                            "EventSource": x.event_source(), "Username": x.username(),
                            "Resources": x.resources().iter().map(|r| json!({"ResourceType": r.resource_type(), "ResourceName": r.resource_name()})).collect::<Vec<_>>(),
                            "CloudTrailEvent": x.cloud_trail_event(),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }

        // ---------- Lambda ----------
        ("lambda", "list-functions") => {
            let c = aws_sdk_lambda::Client::new(&cfg);
            listed("Functions", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.list_functions().set_marker(t).send().await.map_err(e!())?;
                    // Environment variables are left out: their values are never shown.
                    let x = r
                        .functions()
                        .iter()
                        .map(|f| json!({
                            "FunctionName": f.function_name(), "FunctionArn": f.function_arn(), "Runtime": f.runtime().map(|x| x.as_str()),
                            "MemorySize": f.memory_size(), "Timeout": f.timeout(), "CodeSize": f.code_size(), "LastModified": f.last_modified(),
                            "Architectures": f.architectures().iter().map(|x| x.as_str()).collect::<Vec<_>>(), "Handler": f.handler(),
                            "LoggingConfig": f.logging_config().map(|l| json!({"LogGroup": l.log_group()})),
                        }))
                        .collect();
                    Ok((x, r.next_marker().map(String::from)))
                }
            }).await?)
        }

        ("lambda", "get-function-configuration") => {
            let f = aws_sdk_lambda::Client::new(&cfg).get_function_configuration().set_function_name(a.one("--function-name")).send().await.map_err(e!())?;
            // Environment variables are left out: their values are never shown.
            json!({
                "FunctionName": f.function_name(), "FunctionArn": f.function_arn(), "Runtime": f.runtime().map(|x| x.as_str()),
                "Role": f.role(), "Handler": f.handler(), "CodeSize": f.code_size(), "Description": f.description(), "Timeout": f.timeout(),
                "MemorySize": f.memory_size(), "LastModified": f.last_modified(), "CodeSha256": f.code_sha256(), "Version": f.version(),
                "RevisionId": f.revision_id(), "PackageType": f.package_type().map(|x| x.as_str()),
                "Architectures": f.architectures().iter().map(|x| x.as_str()).collect::<Vec<_>>(),
                "EphemeralStorage": f.ephemeral_storage().map(|e| json!({"Size": e.size()})),
                "Layers": f.layers().iter().map(|l| json!({"Arn": l.arn(), "CodeSize": l.code_size()})).collect::<Vec<_>>(),
                "VpcConfig": f.vpc_config().filter(|v| v.vpc_id().is_some_and(|x| !x.is_empty())).map(|v| json!({
                    "VpcId": v.vpc_id(), "SubnetIds": v.subnet_ids(), "SecurityGroupIds": v.security_group_ids(),
                })),
                "State": f.state().map(|x| x.as_str()), "StateReason": f.state_reason(), "StateReasonCode": f.state_reason_code().map(|x| x.as_str()),
                "LastUpdateStatus": f.last_update_status().map(|x| x.as_str()), "LastUpdateStatusReason": f.last_update_status_reason(),
                "LastUpdateStatusReasonCode": f.last_update_status_reason_code().map(|x| x.as_str()),
                "LoggingConfig": f.logging_config().map(|l| json!({"LogGroup": l.log_group()})),
            })
        }

        // ---------- Secrets Manager ----------
        ("secretsmanager", "list-secrets") => {
            let c = aws_sdk_secretsmanager::Client::new(&cfg);
            listed("SecretList", pages(max, |t| {
                let c = c.clone();
                async move {
                    let r = c.list_secrets().set_next_token(t).send().await.map_err(e!())?;
                    let x = r
                        .secret_list()
                        .iter()
                        .map(|x| json!({
                            "Name": x.name(), "ARN": x.arn(), "Description": x.description(), "KmsKeyId": x.kms_key_id(),
                            "RotationEnabled": x.rotation_enabled(), "LastRotatedDate": ts(x.last_rotated_date()),
                            "NextRotationDate": ts(x.next_rotation_date()), "LastChangedDate": ts(x.last_changed_date()),
                            "LastAccessedDate": ts(x.last_accessed_date()), "CreatedDate": ts(x.created_date()),
                            "OwningService": x.owning_service(), "PrimaryRegion": x.primary_region(),
                            "Tags": x.tags().iter().map(|t| json!({"Key": t.key(), "Value": t.value()})).collect::<Vec<_>>(),
                        }))
                        .collect();
                    Ok((x, r.next_token().map(String::from)))
                }
            }).await?)
        }
        // --write only (aws.rs). The value comes back as is: e9s shows it only when asked (x).
        ("secretsmanager", "get-secret-value") => {
            let r = aws_sdk_secretsmanager::Client::new(&cfg).get_secret_value().set_secret_id(a.one("--secret-id")).send().await.map_err(e!())?;
            json!({
                "Name": r.name(), "ARN": r.arn(), "VersionId": r.version_id(), "SecretString": r.secret_string(),
                "SecretBinary": r.secret_binary().map(|b| format!("<{} bytes of binary>", b.as_ref().len())), "CreatedDate": ts(r.created_date()),
            })
        }
        ("secretsmanager", "put-secret-value") => {
            let r = aws_sdk_secretsmanager::Client::new(&cfg)
                .put_secret_value()
                .set_secret_id(a.one("--secret-id"))
                .set_secret_string(a.raw("--secret-string"))
                .send()
                .await
                .map_err(e!())?;
            json!({"Name": r.name(), "ARN": r.arn(), "VersionId": r.version_id()})
        }

        _ => return Err(AwsError::Api(format!("e9s does not call aws {service} {op}"))),
    })
}

// ---------- shapes shared by several operations ----------

fn ec2_instance(i: &aws_sdk_ec2::types::Instance) -> Value {
    json!({
        "InstanceId": i.instance_id(), "InstanceType": i.instance_type().map(|x| x.as_str()),
        "State": {"Name": i.state().and_then(|s| s.name()).map(|x| x.as_str())},
        "Placement": {"AvailabilityZone": i.placement().and_then(|p| p.availability_zone())},
        "PrivateIpAddress": i.private_ip_address(), "PublicIpAddress": i.public_ip_address(), "LaunchTime": ts(i.launch_time()),
        "ImageId": i.image_id(), "VpcId": i.vpc_id(), "SubnetId": i.subnet_id(),
        "Tags": i.tags().iter().map(|t| json!({"Key": t.key(), "Value": t.value()})).collect::<Vec<_>>(),
    })
}

fn ecs_failures(f: &[aws_sdk_ecs::types::Failure]) -> Value {
    json!(f.iter().map(|x| json!({"arn": x.arn(), "reason": x.reason(), "detail": x.detail()})).collect::<Vec<_>>())
}

fn ecs_cluster(c: &aws_sdk_ecs::types::Cluster) -> Value {
    json!({
        "clusterName": c.cluster_name(), "clusterArn": c.cluster_arn(), "status": c.status(),
        "activeServicesCount": c.active_services_count(), "runningTasksCount": c.running_tasks_count(),
        "pendingTasksCount": c.pending_tasks_count(), "registeredContainerInstancesCount": c.registered_container_instances_count(),
        "capacityProviders": c.capacity_providers(),
        "settings": c.settings().iter().map(|s| json!({"name": s.name().map(|x| x.as_str()), "value": s.value()})).collect::<Vec<_>>(),
    })
}

fn ecs_service(v: &aws_sdk_ecs::types::Service) -> Value {
    let dc = v.deployment_configuration();
    json!({
        "serviceName": v.service_name(), "serviceArn": v.service_arn(), "clusterArn": v.cluster_arn(), "status": v.status(),
        "desiredCount": v.desired_count(), "runningCount": v.running_count(), "pendingCount": v.pending_count(),
        "launchType": v.launch_type().map(|x| x.as_str()), "platformVersion": v.platform_version(), "taskDefinition": v.task_definition(),
        "schedulingStrategy": v.scheduling_strategy().map(|x| x.as_str()), "createdAt": ts(v.created_at()),
        "healthCheckGracePeriodSeconds": v.health_check_grace_period_seconds(),
        "deploymentController": v.deployment_controller().map(|d| json!({"type": d.r#type().as_str()})),
        "deploymentConfiguration": dc.map(|d| json!({
            "maximumPercent": d.maximum_percent(), "minimumHealthyPercent": d.minimum_healthy_percent(),
            "deploymentCircuitBreaker": d.deployment_circuit_breaker().map(|b| json!({"enable": b.enable(), "rollback": b.rollback()})),
        })),
        "networkConfiguration": v.network_configuration().and_then(|n| n.awsvpc_configuration()).map(|n| json!({"awsvpcConfiguration": {
            "subnets": n.subnets(), "securityGroups": n.security_groups(), "assignPublicIp": n.assign_public_ip().map(|x| x.as_str()),
        }})),
        "loadBalancers": v.load_balancers().iter().map(|l| json!({
            "targetGroupArn": l.target_group_arn(), "containerName": l.container_name(), "containerPort": l.container_port(),
        })).collect::<Vec<_>>(),
        "deployments": v.deployments().iter().map(|d| json!({
            "id": d.id(), "status": d.status(), "taskDefinition": d.task_definition(), "desiredCount": d.desired_count(),
            "runningCount": d.running_count(), "pendingCount": d.pending_count(), "failedTasks": d.failed_tasks(),
            "rolloutState": d.rollout_state().map(|x| x.as_str()), "rolloutStateReason": d.rollout_state_reason(),
            "createdAt": ts(d.created_at()), "updatedAt": ts(d.updated_at()),
        })).collect::<Vec<_>>(),
        "events": v.events().iter().map(|e| json!({"id": e.id(), "createdAt": ts(e.created_at()), "message": e.message()})).collect::<Vec<_>>(),
    })
}

fn ecs_task(t: &aws_sdk_ecs::types::Task) -> Value {
    json!({
        "taskArn": t.task_arn(), "clusterArn": t.cluster_arn(), "group": t.group(), "startedBy": t.started_by(),
        "taskDefinitionArn": t.task_definition_arn(), "lastStatus": t.last_status(), "desiredStatus": t.desired_status(),
        "healthStatus": t.health_status().map(|x| x.as_str()), "stopCode": t.stop_code().map(|x| x.as_str()), "stoppedReason": t.stopped_reason(),
        "launchType": t.launch_type().map(|x| x.as_str()), "platformVersion": t.platform_version(), "capacityProviderName": t.capacity_provider_name(),
        "availabilityZone": t.availability_zone(), "connectivity": t.connectivity().map(|x| x.as_str()), "cpu": t.cpu(), "memory": t.memory(),
        "createdAt": ts(t.created_at()), "pullStartedAt": ts(t.pull_started_at()), "pullStoppedAt": ts(t.pull_stopped_at()),
        "startedAt": ts(t.started_at()), "stoppingAt": ts(t.stopping_at()), "stoppedAt": ts(t.stopped_at()),
        "executionStoppedAt": ts(t.execution_stopped_at()),
        "attachments": t.attachments().iter().map(|x| json!({
            "id": x.id(), "type": x.r#type(), "status": x.status(),
            "details": x.details().iter().map(|d| json!({"name": d.name(), "value": d.value()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "containers": t.containers().iter().map(|c| json!({
            "name": c.name(), "image": c.image(), "imageDigest": c.image_digest(), "runtimeId": c.runtime_id(), "lastStatus": c.last_status(),
            "exitCode": c.exit_code(), "reason": c.reason(), "healthStatus": c.health_status().map(|x| x.as_str()), "cpu": c.cpu(),
            "memory": c.memory(),
            "networkInterfaces": c.network_interfaces().iter().map(|n| json!({"privateIpv4Address": n.private_ipv4_address()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn ecs_taskdef(d: &aws_sdk_ecs::types::TaskDefinition) -> Value {
    json!({
        "taskDefinitionArn": d.task_definition_arn(), "family": d.family(), "revision": d.revision(), "status": d.status().map(|x| x.as_str()),
        "cpu": d.cpu(), "memory": d.memory(), "networkMode": d.network_mode().map(|x| x.as_str()), "registeredAt": ts(d.registered_at()),
        "requiresCompatibilities": d.requires_compatibilities().iter().map(|x| x.as_str()).collect::<Vec<_>>(),
        "containerDefinitions": d.container_definitions().iter().map(|c| json!({
            "name": c.name(), "image": c.image(), "cpu": c.cpu(), "memory": c.memory(), "memoryReservation": c.memory_reservation(),
            "essential": c.essential(), "command": c.command(), "entryPoint": c.entry_point(), "stopTimeout": c.stop_timeout(),
            "portMappings": c.port_mappings().iter().map(|p| json!({
                "containerPort": p.container_port(), "hostPort": p.host_port(), "protocol": p.protocol().map(|x| x.as_str()),
            })).collect::<Vec<_>>(),
            "healthCheck": c.health_check().map(|h| json!({
                "command": h.command(), "interval": h.interval(), "timeout": h.timeout(), "retries": h.retries(), "startPeriod": h.start_period(),
            })),
            "logConfiguration": c.log_configuration().map(|l| json!({"logDriver": l.log_driver().as_str(), "options": l.options()})),
            // Names only: values are never read into the UI.
            "environment": c.environment().iter().map(|e| json!({"name": e.name(), "value": REDACTED})).collect::<Vec<_>>(),
            "secrets": c.secrets().iter().map(|s| json!({"name": s.name(), "valueFrom": REDACTED})).collect::<Vec<_>>(),
            "dependsOn": c.depends_on().iter().map(|x| json!({"containerName": x.container_name(), "condition": x.condition().as_str()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn target_group(tg: &aws_sdk_elasticloadbalancingv2::types::TargetGroup) -> Value {
    json!({
        "TargetGroupArn": tg.target_group_arn(), "TargetGroupName": tg.target_group_name(), "Protocol": tg.protocol().map(|x| x.as_str()),
        "Port": tg.port(), "TargetType": tg.target_type().map(|x| x.as_str()), "VpcId": tg.vpc_id(), "LoadBalancerArns": tg.load_balancer_arns(),
        "HealthCheckProtocol": tg.health_check_protocol().map(|x| x.as_str()), "HealthCheckPort": tg.health_check_port(),
        "HealthCheckPath": tg.health_check_path(), "HealthCheckIntervalSeconds": tg.health_check_interval_seconds(),
        "HealthCheckTimeoutSeconds": tg.health_check_timeout_seconds(), "HealthyThresholdCount": tg.healthy_threshold_count(),
        "UnhealthyThresholdCount": tg.unhealthy_threshold_count(), "Matcher": tg.matcher().map(|m| json!({"HttpCode": m.http_code()})),
    })
}

fn scaling_policy(p: &aws_sdk_applicationautoscaling::types::ScalingPolicy) -> Value {
    let c = p.target_tracking_scaling_policy_configuration();
    json!({
        "PolicyARN": p.policy_arn(), "PolicyName": p.policy_name(), "ResourceId": p.resource_id(), "PolicyType": p.policy_type().as_str(),
        "ServiceNamespace": p.service_namespace().as_str(), "ScalableDimension": p.scalable_dimension().as_str(),
        "TargetTrackingScalingPolicyConfiguration": c.map(|c| json!({
            "TargetValue": c.target_value(), "ScaleInCooldown": c.scale_in_cooldown(), "ScaleOutCooldown": c.scale_out_cooldown(),
            "DisableScaleIn": c.disable_scale_in(),
            "PredefinedMetricSpecification": c.predefined_metric_specification().map(|m| json!({"PredefinedMetricType": m.predefined_metric_type().as_str()})),
            "CustomizedMetricSpecification": c.customized_metric_specification().map(|m| json!({"MetricName": m.metric_name()})),
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_read_back_and_max_items_cut_with_a_token() {
        let v: Vec<String> = ["--cluster", "c", "--tasks", "a", "b", "--descending", "--max-items", "3"].map(String::from).to_vec();
        let a = Args(&v);
        assert_eq!(a.all("--tasks"), ["a", "b"]);
        assert_eq!(a.one("--cluster").as_deref(), Some("c"));
        assert!(a.has("--descending") && a.max_items() == Some(3));

        // Two pages of 2 with a token after each: stops at 3 items and says more exist.
        let calls = std::cell::Cell::new(0);
        let (items, more) = runtime().block_on(pages(Some(3), |_| {
            calls.set(calls.get() + 1);
            async { Ok((vec![json!(1), json!(2)], Some("t".to_string()))) }
        })).unwrap();
        assert_eq!((items.len(), more, calls.get()), (3, true, 2));
        let v = listed("events", (items, more));
        assert!(crate::aws::truncated(&v), "{v}");

        let t = aws_smithy_types::DateTime::from_fractional_secs(1_760_513_226, 0.476999);
        assert_eq!(ts(Some(&t)), json!("2025-10-15T07:27:06.477Z"));

        let mut v = json!({"a": null, "b": [{"c": null, "d": 1}]});
        prune(&mut v);
        assert_eq!(v, json!({"b": [{"d": 1}]}));
    }
}
