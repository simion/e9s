//! "About this view" in the help overlay: the AWS concept behind each screen, mapped to Kubernetes.

use crate::views::View;

pub fn about(view: &View) -> (&'static str, &'static [&'static str]) {
    match view {
        View::Profiles => ("Profiles", &[
            "A profile is a named way to get credentials, kept in ~/.aws/config. Kubernetes: a kubeconfig context.",
            "With AWS SSO a profile names an account and a role in it (for example ReadOnlyAccess); `aws sso login` refreshes it.",
            "Accounts are the hard isolation boundary in AWS, closer to separate Kubernetes clusters than to namespaces.",
        ]),
        View::Regions => ("Regions", &[
            "A region (us-west-1, eu-west-1...) is a separate set of data centres. Almost every resource lives in exactly one region,",
            "and every API call is made against one region. Kubernetes: think of each region as its own cluster API server.",
            "This scan asks every region you can use for its ECS clusters, so you can see where things actually run.",
        ]),
        View::Clusters => ("ECS clusters", &[
            "ECS (Elastic Container Service) is AWS's own container orchestrator, the Kubernetes of this world.",
            "An ECS cluster groups services and tasks. Kubernetes: a cluster, used much like a namespace.",
            "With Fargate there are no nodes to manage: AWS runs each task on its own capacity, so INSTANCES is often 0.",
            "SERVICES = long-running services, RUNNING / PENDING = tasks (pods) in those states.",
        ]),
        View::Services { .. } => ("ECS services", &[
            "A service keeps DESIRED copies of a task definition running. Kubernetes: a Deployment.",
            "Each rollout creates a new deployment inside the service. Kubernetes: a new ReplicaSet.",
            "PRIMARY is the new generation, ACTIVE the old one being replaced (old ReplicaSet scaling down).",
            "RUN(ALL) is ECS's runningCount: new + old generations added up, so it can exceed DESIRED mid-rollout.",
            "NEW = new-generation tasks running / DESIRED: the real rollout progress. OLD = what the old generation still runs.",
            "TASKDEF = family:revision of the task definition the new generation runs. Kubernetes: the pod template version.",
            "ROLLOUT = rolloutState of the new generation: IN_PROGRESS, COMPLETED or FAILED (the circuit breaker rolled back).",
        ]),
        View::Tasks { .. } => ("ECS tasks", &[
            "A task is one running copy of a task definition: one or more containers scheduled together. Kubernetes: a Pod.",
            "LAST is where the task is (PROVISIONING, PENDING, RUNNING, STOPPED); DESIRED is where ECS wants it. Kubernetes: pod phase.",
            "GEN says which deployment started it: new (PRIMARY) or old (ACTIVE).",
            "HEALTH comes from the container health check in the task definition. Kubernetes: liveness probe.",
            "TARGET is the load balancer's view of the task's IP. Kubernetes: readiness as seen by the Service.",
            "Stopped tasks stay visible for about an hour, then ECS forgets them. Kubernetes: like failed pods getting garbage-collected.",
            "REASON gathers why it stopped: stopCode and stoppedReason (the pod's termination reason), container exit codes,",
            "and the failed load balancer health check if that is what killed it. 137 = SIGKILL (often out of memory), 143 = SIGTERM.",
        ]),
        View::Containers { .. } => ("Containers of a task", &[
            "The containers inside one task. Kubernetes: the containers of a pod (sidecars included, like the GuardDuty agent).",
            "EXIT is the container's exit code once it stopped; REASON is ECS's explanation (OutOfMemoryError, CannotPullContainer...).",
            "l opens this container's CloudWatch logs. Kubernetes: kubectl logs <pod> -c <container>.",
        ]),
        View::Events { .. } => ("Service events", &[
            "ECS writes what the service scheduler did as events, newest first, up to the last 100. Kubernetes: the events in",
            "kubectl describe deployment. Look for: has started / has stopped N tasks, registered / deregistered targets,",
            "is unhealthy in target-group (failed health check), has reached a steady state (rollout settled).",
        ]),
        View::Failures { .. } => ("Failures of the last hour", &[
            "Tasks that stopped because something failed (load balancer health check, failed start, crash or non-zero exit),",
            "and service events reporting a problem (unhealthy target, failed placement, failed launch), newest first.",
            "Drains of the old generation during a rollout are normal and left out. Kubernetes: pods in CrashLoopBackOff or",
            "OOMKilled plus Warning events (kubectl get events --field-selector type=Warning).",
            "ECS keeps stopped tasks for only about an hour, so this is the window. The services list counts it in FAILED 1H.",
        ]),
        View::Rollout { .. } => ("Rollout watch", &[
            "Kubernetes: kubectl rollout status plus watching both ReplicaSets and their pods.",
            "PROGRESS = new-generation running / the service's desired count. ECS ramps the new deployment's own desired count",
            "during the roll, so that number is not the goal; the service desiredCount is.",
            "Target group counts: initial = registering, health checks not yet passed; draining = deregistering and finishing",
            "in-flight requests for the deregistration delay. Kubernetes: a terminating pod in its grace period.",
            "CHURN separates normal drains of the old generation from failures, so a slow roll and a crash loop look different.",
        ]),
        View::TaskDefFamilies | View::TaskDefRevisions { .. } | View::TaskDefContainers { .. } => ("Task definitions", &[
            "A task definition describes what to run: images, CPU and memory, ports, env var names, log settings, IAM role.",
            "Kubernetes: a pod spec / pod template. Each change registers a new immutable revision (family:revision);",
            "a service points at one revision, so deploying means pointing the service at a newer one.",
            "Env var and secret values are never shown, only their names.",
        ]),
        View::LogGroups | View::LogStreams { .. } | View::Logs { .. } => ("CloudWatch Logs", &[
            "CloudWatch Logs stores container output. A log group is a destination (often one per service); a log stream is one",
            "source inside it. With the awslogs driver each container of each task writes its own stream: <prefix>/<container>/<task-id>.",
            "Kubernetes: kubectl logs <pod> -c <container>, except the logs outlive the task.",
            "Logs follow new lines by default (f pauses). A service's logs merge every running task's stream by time, each line",
            "tagged with its task id in its own colour. Kubernetes: kubectl logs -f deployment/<name>, or k9s logs on a deployment.",
            "/ filters and highlights lines on screen; p sets a CloudWatch filter pattern (server-side search, reloads).",
        ]),
        View::LoadBalancers => ("Load balancers", &[
            "An Application Load Balancer (ALB) routes HTTP by host and path to target groups. Kubernetes: an Ingress and its controller.",
            "A Network Load Balancer (NLB) forwards TCP/UDP. Kubernetes: a Service of type LoadBalancer.",
            "m shows request count, 5xx errors and p95 latency.",
        ]),
        View::TargetGroups { .. } | View::Targets { .. } => ("Target groups", &[
            "A target group is the set of IP:port targets a load balancer sends traffic to, with its health check.",
            "Kubernetes: a Service's Endpoints. ECS registers each task's IP when it starts and deregisters it when it stops.",
            "States: healthy, unhealthy (failing the health check), initial (registering), draining (deregistering),",
            "unused (not attached to a listener). Deregistration delay is how long draining lasts.",
            "Slow start ramps traffic to a new target gradually; the algorithm picks targets (round robin or least outstanding requests).",
        ]),
        View::ScalableTargets { .. } | View::ScalingPolicies { .. } | View::ScalingActivities { .. } => ("Application Auto Scaling", &[
            "Application Auto Scaling changes a number (an ECS service's desired count, for example) between MIN and MAX.",
            "Kubernetes: a HorizontalPodAutoscaler with minReplicas and maxReplicas.",
            "Policies say when: target tracking keeps a metric near a value (like an HPA CPU target); step scaling reacts to alarms.",
            "Activities are what it did and why. Kubernetes: the HPA's events. Suspended means scaling is paused.",
        ]),
        View::Metrics { .. } => ("CloudWatch metrics", &[
            "CloudWatch Metrics is AWS's time-series store. Kubernetes: Prometheus / metrics-server.",
            "ECS CPU and memory are a percentage of what the service reserved, not of the machine.",
            "p95 = the latency 95% of requests beat. Each point is a 5-minute period.",
        ]),
        View::Ec2 => ("EC2 instances", &[
            "An EC2 instance is a virtual machine. Kubernetes: a Node. ECS on Fargate does not need any.",
        ]),
        View::EcrRepos | View::EcrImages { .. } => ("ECR", &[
            "Elastic Container Registry stores container images. Kubernetes: the registry your pod images come from.",
            "A repository holds images; tags point at digests, and a task definition's image names one of them.",
        ]),
        View::Sqs => ("SQS queues", &[
            "Simple Queue Service is a managed message queue. No Kubernetes built-in; think RabbitMQ run for you.",
            "VISIBLE = waiting to be received; IN FLIGHT = received by a worker, not yet deleted; OLDEST = age of the oldest message.",
            "DLQ = the dead-letter queue that messages move to after too many failed attempts.",
        ]),
        View::Rds => ("RDS", &[
            "Relational Database Service runs Postgres or MySQL for you. Kubernetes: a database StatefulSet plus an operator.",
            "Aurora clusters have one writer instance and reader replicas.",
        ]),
        View::Lambda => ("Lambda", &[
            "Lambda runs a function per event with no servers or pods to see. Kubernetes: Knative or another serverless layer.",
            "l opens its log group, /aws/lambda/<name> by default.",
        ]),
        View::Detail { .. } => ("Describe", &[
            "The full object as AWS returned it. Kubernetes: kubectl get -o yaml / kubectl describe.",
        ]),
    }
}
