//! A scripted troubleshooting session for evaluating context summaries over several rounds of
//! compaction. Each round runs a
//! few commands; the commands, their output and what the assistant says plant facts among
//! routine noise. The round ends with the user's next request, and a summary follows.
//!
//! Each summary is scored by the planted facts that still hold and that its facts state; by
//! the replaced ones that it gives only the old value of; and by the replaced ones that some
//! fact states without the replacement, as an account of what happened earlier also does. The
//! session is synthetic: it stands in for a recorded one.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use zx_agent::ai::{
    AssistantMessage, ContentBlock, Message, Provider, StopReason, ToolCall, ToolResultMessage, Usage, now_ms,
};
use zx_agent::compaction::{Config, Outcome};
use zx_agent::session::{Entry, RequestSource, SessionStore};

use super::{RunnerSetup, config, header};

/// Rounds in the scripted session; a summary follows each.
pub(crate) const ROUNDS: usize = 8;

/// Lines of routine access log in each round, most of a round's bytes.
const LOG_LINES: usize = 100;

/// What the user asks before the first round.
const TASK: &str = "Checkout in namespace shop has been failing and slow since about 08:10 UTC. Find the cause, fix it with the smallest safe changes, and tell me the follow-ups.";

/// A fact planted in the records of one round.
pub(crate) struct Planted {
    pub id: &'static str,
    /// The round whose records first state it.
    pub round: usize,
    /// The fact as a summary might state it.
    pub text: &'static str,
    /// Lowercase. A text states the fact when it contains, ignoring case, an alternative of
    /// every group.
    pub keys: &'static [&'static [&'static str]],
    /// The planted fact that makes this one obsolete, such as a changed setting.
    pub replaced_by: Option<&'static str>,
}

impl Planted {
    /// Whether `text` states this fact.
    pub(crate) fn stated_in(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.keys.iter().all(|group| group.iter().any(|key| text.contains(key)))
    }

    /// The planted fact that replaces this one.
    pub(crate) fn replacement(&self) -> Option<&'static Planted> {
        self.replaced_by.map(planted)
    }

    /// Whether the fact holds after `round`: stated by then, and not yet replaced.
    pub(crate) fn holds_after(&self, round: usize) -> bool {
        self.round <= round && self.replacement().is_none_or(|by| by.round > round)
    }

    /// Whether, after `round`, the fact is replaced and `facts` give only its old value: one
    /// states it, and none states its replacement.
    fn stale_in(&self, round: usize, facts: &[String]) -> bool {
        self.replacement().is_some_and(|by| {
            by.round <= round
                && facts.iter().any(|text| self.stated_in(text))
                && !facts.iter().any(|text| by.stated_in(text))
        })
    }

    /// Whether, after `round`, the fact is replaced and one of `facts` states it without
    /// stating its replacement too, as an account of what happened earlier does.
    fn old_value_in(&self, round: usize, facts: &[String]) -> bool {
        self.replacement()
            .is_some_and(|by| by.round <= round && facts.iter().any(|text| self.stated_in(text) && !by.stated_in(text)))
    }
}

/// The planted fact with this ID.
pub(crate) fn planted(id: &str) -> &'static Planted {
    PLANTED
        .iter()
        .find(|fact| fact.id == id)
        .unwrap_or_else(|| panic!("no planted fact {id}"))
}

/// Three facts per round; six of them are replaced later.
pub(crate) static PLANTED: &[Planted] = &[
    Planted {
        id: "F1",
        round: 1,
        text: "Pod checkout-api-6f7c9d-x2kqp is in CrashLoopBackOff with 37 restarts",
        keys: &[&["x2kqp"], &["crashloop", "crash loop", "crash-loop", "crashing"]],
        replaced_by: Some("F14"),
    },
    Planted {
        id: "F2",
        round: 1,
        text: "The crash-looping pod runs on node ip-10-42-7-19",
        keys: &[&["ip-10-42-7-19"]],
        // The pod is gone once the rollout replaces it.
        replaced_by: Some("F14"),
    },
    Planted {
        id: "F3",
        round: 1,
        text: "checkout-api runs image registry.internal/shop/checkout-api:2.14.3",
        keys: &[&["2.14.3"]],
        replaced_by: None,
    },
    Planted {
        id: "F4",
        round: 2,
        text: "The checkout-api container was OOMKilled (exit code 137)",
        keys: &[&[
            "oomkilled",
            "oom-killed",
            "oom killed",
            "out of memory",
            "out-of-memory",
            "outofmemory",
        ]],
        replaced_by: None,
    },
    Planted {
        id: "F5",
        round: 2,
        text: "The checkout-api memory limit is 512Mi",
        keys: &[&["512"]],
        replaced_by: Some("F13"),
    },
    Planted {
        id: "F6",
        round: 2,
        text: "checkout-api starts the JVM with -Xmx768m",
        keys: &[&["768"]],
        replaced_by: None,
    },
    Planted {
        id: "F7",
        round: 3,
        text: "db-primary-2 allows at most 200 connections (max_connections)",
        keys: &[&["db-primary-2"], &["200"]],
        replaced_by: None,
    },
    Planted {
        id: "F8",
        round: 3,
        text: "All 24 connections of the checkout connection pool are in use",
        keys: &[&["pool", "hikari"], &["24"]],
        replaced_by: None,
    },
    Planted {
        id: "F9",
        round: 3,
        text: "POST /api/checkout p99 latency is 1840 ms",
        keys: &[&["1840", "1.84", "1,840"]],
        replaced_by: Some("F21"),
    },
    Planted {
        id: "F10",
        round: 4,
        text: "ASYNC_TAX_CALC is false: tax is calculated synchronously",
        keys: &[&["async_tax_calc"], &["false", "disabled"]],
        replaced_by: Some("F16"),
    },
    Planted {
        id: "F11",
        round: 4,
        text: "Calls to tax-service time out after 2500 ms",
        keys: &[
            &["tax-service", "tax service", "tax_service"],
            &["2500", "2.5", "2,500"],
        ],
        replaced_by: None,
    },
    Planted {
        id: "F12",
        round: 4,
        text: "Pod tax-service-5d8b7-qq4zt is not ready: its readiness probe returns 503",
        keys: &[
            &["qq4zt"],
            &["not ready", "notready", "0/1", "503", "unready", "readiness"],
        ],
        replaced_by: Some("F17"),
    },
    Planted {
        id: "F13",
        round: 5,
        text: "The checkout-api memory limit was raised from 512Mi to 1536Mi",
        keys: &[&["1536"]],
        replaced_by: None,
    },
    Planted {
        id: "F14",
        round: 5,
        text: "New pod checkout-api-8d4b2f-r8m2v runs with 0 restarts",
        keys: &[&["r8m2v"]],
        replaced_by: None,
    },
    Planted {
        id: "F15",
        round: 5,
        text: "The memory change is tracked as CHG-30517",
        keys: &[&["chg-30517"]],
        replaced_by: None,
    },
    Planted {
        id: "F16",
        round: 6,
        text: "ASYNC_TAX_CALC was set to true",
        keys: &[&["async_tax_calc"], &["true", "enabled"]],
        replaced_by: None,
    },
    Planted {
        id: "F17",
        round: 6,
        text: "Pod tax-service-5d8b7-h7c2n replaced tax-service-5d8b7-qq4zt and is ready",
        keys: &[&["h7c2n"]],
        replaced_by: None,
    },
    Planted {
        id: "F18",
        round: 6,
        text: "Alert CheckoutHighLatency has fired since 08:12 UTC",
        keys: &[&["checkouthighlatency"]],
        replaced_by: None,
    },
    Planted {
        id: "F19",
        round: 7,
        text: "Queries on orders by customer_id used a sequential scan",
        keys: &[
            &["orders"],
            &["seq scan", "seqscan", "sequential scan", "full scan", "full table scan"],
        ],
        replaced_by: None,
    },
    Planted {
        id: "F20",
        round: 7,
        text: "Index idx_orders_customer_id was created on orders (customer_id)",
        keys: &[&["idx_orders_customer_id"]],
        replaced_by: None,
    },
    Planted {
        id: "F21",
        round: 7,
        text: "POST /api/checkout p99 latency dropped to 310 ms",
        keys: &[&["310"]],
        replaced_by: None,
    },
    Planted {
        id: "F22",
        round: 8,
        text: "The incident is INC-4821",
        keys: &[&["inc-4821"]],
        replaced_by: None,
    },
    Planted {
        id: "F23",
        round: 8,
        text: "Team payments-core owns the incident",
        keys: &[&["payments-core"]],
        replaced_by: None,
    },
    Planted {
        id: "F24",
        round: 8,
        text: "checkout-api memory peaks at 1121Mi, about 73% of its new limit",
        keys: &[&["1121", "73%", "73 %", "73 percent"]],
        replaced_by: None,
    },
];

/// One step of a round.
enum Step {
    /// The assistant runs a command; its output is the tool result.
    Run(&'static str, String),
    /// The assistant says something.
    Say(&'static str),
    /// The user's next request, which ends the round.
    Ask(&'static str),
}

/// The messages of round `n`, from 1 to [`ROUNDS`].
#[expect(clippy::too_many_lines, reason = "a scripted conversation, one round per arm")]
pub(crate) fn round(n: usize) -> Vec<Message> {
    let steps = match n {
        1 => vec![
            Step::Run("kubectl get pods -n shop -o wide", PODS.into()),
            Step::Run(
                "kubectl get events -n shop --sort-by=.lastTimestamp | tail -n 13",
                EVENTS.into(),
            ),
            Step::Run(
                "kubectl logs deploy/checkout-api -n shop --since=5m",
                access_log("checkout-api", n),
            ),
            Step::Say(
                "checkout-api-6f7c9d-x2kqp is in CrashLoopBackOff; the other two replicas serve traffic, slowly.",
            ),
            Step::Ask("Find out why that pod keeps restarting."),
        ],
        2 => vec![
            Step::Run(
                "kubectl describe pod checkout-api-6f7c9d-x2kqp -n shop",
                DESCRIBE.into(),
            ),
            Step::Run(
                "kubectl logs checkout-api-6f7c9d-x2kqp -n shop --previous --tail=104",
                access_log("checkout-api", n) + OUT_OF_MEMORY,
            ),
            Step::Say("The container is OOMKilled: the JVM heap (-Xmx768m) does not fit in the 512Mi memory limit."),
            Step::Ask("Before changing anything, check whether the database is a problem too."),
        ],
        3 => vec![
            Step::Run("./scripts/db-report.sh db-primary-2", DB_REPORT.into()),
            Step::Run(
                "curl -s http://checkout-api.shop:9090/metrics | grep -E '^(hikaricp_connections|jvm_memory_used_bytes)'",
                METRICS.into(),
            ),
            Step::Run(
                "latency-report --service checkout-api --window 15m",
                LATENCY_BEFORE.into(),
            ),
            Step::Run(
                "kubectl logs deploy/payments -n shop --since=5m",
                access_log("payments", n),
            ),
            Step::Say(
                "Checkout requests wait for database connections: all 24 pool connections are busy, and POST /api/checkout p99 is 1840 ms.",
            ),
            Step::Ask("Look at checkout-api's configuration and the services it calls."),
        ],
        4 => vec![
            Step::Run("kubectl get configmap checkout-config -n shop -o yaml", CONFIG.into()),
            Step::Run(
                "kubectl get pods -n shop -l app=tax-service && kubectl describe pod tax-service-5d8b7-qq4zt -n shop | tail -n 4",
                TAX_PODS.into(),
            ),
            Step::Run(
                "kubectl logs deploy/tax-service -n shop --since=5m",
                access_log("tax-service", n),
            ),
            Step::Say(
                "Tax calculation is synchronous (ASYNC_TAX_CALC is false) with a 2500 ms timeout, and one tax-service pod is not ready.",
            ),
            Step::Ask("Fix the memory limit first; the change ticket is approved."),
        ],
        5 => vec![
            Step::Run(
                "kubectl annotate deploy/checkout-api -n shop change-ticket=CHG-30517 --overwrite && kubectl set resources deploy/checkout-api -n shop -c checkout-api --limits=memory=1536Mi && kubectl get deploy checkout-api -n shop -o jsonpath='{.metadata.annotations.change-ticket} {.spec.template.spec.containers[0].resources.limits.memory}'",
                LIMIT_RAISED.into(),
            ),
            Step::Run(
                "kubectl rollout status deploy/checkout-api -n shop && kubectl get pods -n shop -l app=checkout-api",
                ROLLED_OUT.into(),
            ),
            Step::Run(
                "kubectl logs deploy/checkout-api -n shop --since=5m",
                access_log("checkout-api", n),
            ),
            Step::Say(
                "The memory limit is now 1536Mi instead of 512Mi (CHG-30517); the new pods run without restarts.",
            ),
            Step::Ask("Now deal with the tax-service calls."),
        ],
        6 => vec![
            Step::Run(
                r#"kubectl patch configmap checkout-config -n shop --type merge -p '{"data":{"ASYNC_TAX_CALC":"true"}}' && kubectl rollout restart deploy/checkout-api -n shop && kubectl get configmap checkout-config -n shop -o jsonpath='{.data.ASYNC_TAX_CALC}'"#,
                FLAG_SET.into(),
            ),
            Step::Run(
                "kubectl delete pod tax-service-5d8b7-qq4zt -n shop && kubectl get pods -n shop -l app=tax-service",
                TAX_REPLACED.into(),
            ),
            Step::Run(
                "amtool alert query --alertmanager.url=http://alertmanager.monitoring:9093",
                ALERTS.into(),
            ),
            Step::Run(
                "kubectl logs deploy/cart-api -n shop --since=5m",
                access_log("cart-api", n),
            ),
            Step::Say(
                "Tax calls are asynchronous now and the unready tax-service pod was replaced, but CheckoutHighLatency is still firing.",
            ),
            Step::Ask("Checkout is still slow. Check the database queries."),
        ],
        7 => vec![
            Step::Run(
                "psql -h db-primary-2 -U checkout -c 'explain analyze select * from orders where customer_id = 82113'",
                EXPLAIN.into(),
            ),
            Step::Run(
                "psql -h db-primary-2 -U checkout -c 'create index concurrently idx_orders_customer_id on orders (customer_id)'",
                "CREATE INDEX\n".into(),
            ),
            Step::Run(
                "latency-report --service checkout-api --window 15m",
                LATENCY_AFTER.into(),
            ),
            Step::Run(
                "kubectl logs deploy/catalog -n shop --since=5m",
                access_log("catalog", n),
            ),
            Step::Say("The new index removed the sequential scan on orders; checkout p99 is down to 310 ms."),
            Step::Ask("Wrap up: who owns this, and what follow-ups do we need?"),
        ],
        8 => vec![
            Step::Run("incident show INC-4821", INCIDENT.into()),
            Step::Run("kubectl top pods -n shop -l app=checkout-api", TOP.into()),
            Step::Run(
                "kubectl logs deploy/checkout-api -n shop --since=5m",
                access_log("checkout-api", n),
            ),
            Step::Say(
                "Memory now peaks at about 73% of the new limit. Follow-ups: alert when checkout-api memory exceeds 85% of its limit, and revisit the connection pool size.",
            ),
            Step::Ask("Write the incident update."),
        ],
        _ => panic!("the scripted session has rounds 1 to {ROUNDS}, not {n}"),
    };
    steps
        .into_iter()
        .enumerate()
        .flat_map(|(index, step)| messages(n, index, step))
        .collect()
}

fn messages(round: usize, index: usize, step: Step) -> Vec<Message> {
    match step {
        Step::Run(command, output) => {
            let id = format!("r{round}c{index}");
            vec![
                assistant(
                    ContentBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: "bash".into(),
                        arguments: json!({ "command": command }),
                        arguments_error: None,
                    }),
                    StopReason::ToolUse,
                ),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: "bash".into(),
                    content: output,
                    is_error: false,
                    timestamp: now_ms(),
                    duration_ms: Some(350),
                    details: None,
                    usage: None,
                }),
            ]
        }
        Step::Say(text) => vec![assistant(ContentBlock::Text { text: text.into() }, StopReason::Stop)],
        Step::Ask(text) => vec![Message::user(text)],
    }
}

fn assistant(block: ContentBlock, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![block],
        provider: "scripted".into(),
        model: "scripted".into(),
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        provider_error: None,
        timestamp: now_ms(),
        duration_ms: None,
        response_id: None,
        response_model: None,
    })
}

/// The text a message carries: what was said, the commands run, and their output.
pub(crate) fn text_of(message: &Message) -> String {
    match message {
        // The scripted session has no system messages.
        Message::System(_) => String::new(),
        Message::User { content, .. } => content.clone(),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::Thinking { thinking, .. } => thinking.clone(),
                ContentBlock::ToolCall(ToolCall { arguments, .. }) => arguments.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::ToolResult(result) => result.content.clone(),
    }
}

const PATHS: [&str; 5] = [
    "GET /api/cart",
    "GET /api/catalog/items",
    "POST /api/cart/items",
    "GET /api/shipping/quote",
    "GET /actuator/health",
];

/// Routine access log of `service`, the bulk of a round: none of it matters to the incident,
/// and its only numbers of three or more digits are the date, the port and the status 200.
fn access_log(service: &str, round: usize) -> String {
    (0..LOG_LINES)
        .map(|line| {
            // Minutes after 08:00: the log of a later round runs past 09:00.
            let minutes = 8 * 60 + round * 7 + line / 8;
            let (hour, minute) = (minutes / 60, minutes % 60);
            let second = (line * 13 + round) % 60;
            let thread = 1 + (line * 7 + round) % 40;
            let path = PATHS[(line + round) % PATHS.len()];
            let latency = 3 + (line * 37 + round * 11) % 62;
            let request = letters(line * 7919 + round * 104_729);
            format!(
                "2026-11-02T{hour:02}:{minute:02}:{second:02}Z INFO [http-nio-8080-exec-{thread}] {service} - {path} 200 {latency}ms request={request}\n"
            )
        })
        .collect()
}

/// Seven lowercase letters for `n`, so that request IDs carry no digits.
fn letters(mut n: usize) -> String {
    (0..7)
        .map(|_| {
            let letter = b'a' + u8::try_from(n % 26).unwrap();
            n /= 26;
            char::from(letter)
        })
        .collect()
}

const PODS: &str = concat!(
    "NAME                        READY   STATUS             RESTARTS      AGE   IP          NODE\n",
    "cart-api-7c5d8-bqzpw        1/1     Running            0             3d    10.8.1.12   ip-10-42-3-11\n",
    "cart-api-7c5d8-kmnxt        1/1     Running            0             3d    10.8.2.40   ip-10-42-5-23\n",
    "catalog-6b9f4-dwrtz         1/1     Running            0             6d    10.8.1.77   ip-10-42-3-11\n",
    "catalog-6b9f4-lpqhs         1/1     Running            0             6d    10.8.2.93   ip-10-42-5-23\n",
    "checkout-api-6f7c9d-hv9lq   1/1     Running            2             5h    10.8.2.19   ip-10-42-5-23\n",
    "checkout-api-6f7c9d-x2kqp   0/1     CrashLoopBackOff   37 (2m ago)   5h    10.8.3.41   ip-10-42-7-19\n",
    "checkout-api-6f7c9d-zt4wm   1/1     Running            3             5h    10.8.1.58   ip-10-42-3-11\n",
    "payments-5f6d7-gxwrc        1/1     Running            0             9d    10.8.3.66   ip-10-42-7-19\n",
    "payments-5f6d7-nn8vb        1/1     Running            0             9d    10.8.1.21   ip-10-42-3-11\n",
    "shipping-84c9b-fqtzm        1/1     Running            1             4d    10.8.2.71   ip-10-42-5-23\n",
);

const EVENTS: &str = concat!(
    "LAST SEEN   TYPE      REASON              OBJECT                          MESSAGE\n",
    "5h          Normal    Scheduled           pod/checkout-api-6f7c9d-x2kqp   Successfully assigned shop/checkout-api-6f7c9d-x2kqp to ip-10-42-7-19\n",
    "5h          Normal    Pulling             pod/checkout-api-6f7c9d-x2kqp   Pulling image \"registry.internal/shop/checkout-api:2.14.3\"\n",
    "5h          Normal    Pulled              pod/checkout-api-6f7c9d-x2kqp   Successfully pulled image \"registry.internal/shop/checkout-api:2.14.3\" in 2.4s\n",
    "5h          Normal    Scheduled           pod/checkout-api-6f7c9d-zt4wm   Successfully assigned shop/checkout-api-6f7c9d-zt4wm to ip-10-42-3-11\n",
    "5h          Normal    Pulled              pod/checkout-api-6f7c9d-zt4wm   Container image \"registry.internal/shop/checkout-api:2.14.3\" already present on machine\n",
    "5h          Normal    ScalingReplicaSet   deployment/checkout-api         Scaled up replica set checkout-api-6f7c9d to 3\n",
    "4h          Normal    Pulled              pod/shipping-84c9b-fqtzm        Container image \"registry.internal/shop/shipping:3.2.0\" already present on machine\n",
    "4h          Normal    Started             pod/shipping-84c9b-fqtzm        Started container shipping\n",
    "41m         Normal    Killing             pod/catalog-6b9f4-dwrtz         Stopping container catalog-sidecar\n",
    "12m         Normal    Created             pod/checkout-api-6f7c9d-x2kqp   Created container checkout-api\n",
    "12m         Normal    Started             pod/checkout-api-6f7c9d-x2kqp   Started container checkout-api\n",
    "3m          Warning   Unhealthy           pod/checkout-api-6f7c9d-hv9lq   Liveness probe failed: Get \"http://10.8.2.19:8080/actuator/health\": context deadline exceeded\n",
    "2m          Warning   BackOff             pod/checkout-api-6f7c9d-x2kqp   Back-off restarting failed container checkout-api in pod checkout-api-6f7c9d-x2kqp_shop(4c1e7a2b)\n",
);

const DESCRIBE: &str = concat!(
    "Name:             checkout-api-6f7c9d-x2kqp\n",
    "Namespace:        shop\n",
    "Service Account:  checkout-api\n",
    "Node:             ip-10-42-7-19/10.42.7.19\n",
    "Start Time:       Sun, 02 Nov 2026 03:04:11 +0000\n",
    "Labels:           app=checkout-api\n",
    "                  pod-template-hash=6f7c9d\n",
    "Status:           Running\n",
    "IP:               10.8.3.41\n",
    "Controlled By:    ReplicaSet/checkout-api-6f7c9d\n",
    "Containers:\n",
    "  checkout-api:\n",
    "    Image:          registry.internal/shop/checkout-api:2.14.3\n",
    "    Port:           8080/TCP\n",
    "    State:          Waiting\n",
    "      Reason:       CrashLoopBackOff\n",
    "    Last State:     Terminated\n",
    "      Reason:       OOMKilled\n",
    "      Exit Code:    137\n",
    "      Started:      Sun, 02 Nov 2026 08:21:40 +0000\n",
    "      Finished:     Sun, 02 Nov 2026 08:23:02 +0000\n",
    "    Ready:          False\n",
    "    Restart Count:  37\n",
    "    Limits:\n",
    "      cpu:     1\n",
    "      memory:  512Mi\n",
    "    Requests:\n",
    "      cpu:     250m\n",
    "      memory:  384Mi\n",
    "    Liveness:   http-get http://:8080/actuator/health delay=30s timeout=1s period=10s #success=1 #failure=3\n",
    "    Environment:\n",
    "      SPRING_PROFILES_ACTIVE:  prod\n",
    "      JAVA_OPTS:               -Xms256m -Xmx768m -XX:+UseG1GC\n",
    "      DB_HOST:                 db-primary-2\n",
    "Conditions:\n",
    "  Type              Status\n",
    "  Initialized       True\n",
    "  Ready             False\n",
    "  ContainersReady   False\n",
    "  PodScheduled      True\n",
    "QoS Class:        Burstable\n",
    "Events:\n",
    "  Type     Reason   Age                 From     Message\n",
    "  ----     ------   ----                ----     -------\n",
    "  Warning  BackOff  2m (x412 over 5h)   kubelet  Back-off restarting failed container checkout-api\n",
);

const OUT_OF_MEMORY: &str = concat!(
    "2026-11-02T08:23:01Z ERROR [http-nio-8080-exec-9] o.a.c.c.C.[dispatcherServlet] - Servlet.service() threw exception\n",
    "java.lang.OutOfMemoryError: Java heap space\n",
    "\tat java.base/java.util.Arrays.copyOf(Arrays.java:3537)\n",
    "\tat com.shop.checkout.cart.CartSnapshot.serialize(CartSnapshot.java:88)\n",
);

const DB_REPORT: &str = concat!(
    "host: db-primary-2\n",
    "max_connections: 200\n",
    "connections by state:\n",
    "  active                61\n",
    "  idle                 118\n",
    "  idle in transaction    9\n",
    "longest transaction: 4m12s (pid 77341, application checkout-api)\n",
    "replication lag: 0.4s\n",
);

const METRICS: &str = concat!(
    "hikaricp_connections_max{pool=\"checkout\"} 24.0\n",
    "hikaricp_connections_min{pool=\"checkout\"} 10.0\n",
    "hikaricp_connections_active{pool=\"checkout\"} 24.0\n",
    "hikaricp_connections_idle{pool=\"checkout\"} 0.0\n",
    "hikaricp_connections_pending{pool=\"checkout\"} 31.0\n",
    "hikaricp_connections_timeout_total{pool=\"checkout\"} 1907.0\n",
    "jvm_memory_used_bytes{area=\"heap\",id=\"G1 Eden Space\"} 1.1534336E8\n",
    "jvm_memory_used_bytes{area=\"heap\",id=\"G1 Old Gen\"} 3.09329E8\n",
    "jvm_memory_used_bytes{area=\"nonheap\",id=\"Metaspace\"} 1.0213376E8\n",
);

const LATENCY_BEFORE: &str = concat!(
    "ENDPOINT                   COUNT     P50      P95      P99\n",
    "GET /api/cart              18204     21ms     64ms     97ms\n",
    "POST /api/cart/items        6377     29ms     88ms    143ms\n",
    "POST /api/checkout          2291    412ms   1205ms   1840ms\n",
    "GET /api/shipping/quote     4410     33ms     71ms    119ms\n",
    "GET /actuator/health        5400      2ms      6ms     11ms\n",
);

const CONFIG: &str = concat!(
    "apiVersion: v1\n",
    "kind: ConfigMap\n",
    "metadata:\n",
    "  name: checkout-config\n",
    "  namespace: shop\n",
    "  resourceVersion: \"88213907\"\n",
    "data:\n",
    "  ASYNC_TAX_CALC: \"false\"\n",
    "  CART_SERVICE_URL: http://cart-api.shop:8080\n",
    "  CATALOG_SERVICE_URL: http://catalog.shop:8080\n",
    "  CURRENCY_DEFAULT: EUR\n",
    "  HIKARI_MAX_POOL: \"24\"\n",
    "  LOG_FORMAT: json\n",
    "  LOG_LEVEL: INFO\n",
    "  ORDER_ID_PREFIX: ord\n",
    "  PAYMENTS_SERVICE_URL: http://payments.shop:8080\n",
    "  PAYMENTS_TIMEOUT_MS: \"4000\"\n",
    "  SHIPPING_QUOTE_CACHE_SECONDS: \"300\"\n",
    "  SHIPPING_SERVICE_URL: http://shipping.shop:8080\n",
    "  TAX_SERVICE_TIMEOUT_MS: \"2500\"\n",
    "  TAX_SERVICE_URL: http://tax-service.shop:8080\n",
);

const TAX_PODS: &str = concat!(
    "NAME                       READY   STATUS    RESTARTS   AGE\n",
    "tax-service-5d8b7-mfk2d    1/1     Running   0          2d\n",
    "tax-service-5d8b7-qq4zt    0/1     Running   0          2d\n",
    "tax-service-5d8b7-wv6ns    1/1     Running   0          2d\n",
    "Events:\n",
    "  Type     Reason     Age                 From     Message\n",
    "  ----     ------     ----                ----     -------\n",
    "  Warning  Unhealthy  3m (x41 over 2h)    kubelet  Readiness probe failed: HTTP probe failed with statuscode: 503\n",
);

const LIMIT_RAISED: &str = concat!(
    "deployment.apps/checkout-api annotated\n",
    "deployment.apps/checkout-api resource requirements updated\n",
    "CHG-30517 1536Mi\n",
);

const ROLLED_OUT: &str = concat!(
    "Waiting for deployment \"checkout-api\" rollout to finish: 1 of 3 updated replicas are available...\n",
    "Waiting for deployment \"checkout-api\" rollout to finish: 2 of 3 updated replicas are available...\n",
    "deployment \"checkout-api\" successfully rolled out\n",
    "NAME                        READY   STATUS    RESTARTS   AGE\n",
    "checkout-api-8d4b2f-r8m2v   1/1     Running   0          2m\n",
    "checkout-api-8d4b2f-t5pjq   1/1     Running   0          3m\n",
    "checkout-api-8d4b2f-w3nbx   1/1     Running   0          4m\n",
);

const FLAG_SET: &str = concat!(
    "configmap/checkout-config patched\n",
    "deployment.apps/checkout-api restarted\n",
    "true\n",
);

const TAX_REPLACED: &str = concat!(
    "pod \"tax-service-5d8b7-qq4zt\" deleted\n",
    "NAME                       READY   STATUS    RESTARTS   AGE\n",
    "tax-service-5d8b7-h7c2n    1/1     Running   0          40s\n",
    "tax-service-5d8b7-mfk2d    1/1     Running   0          2d\n",
    "tax-service-5d8b7-wv6ns    1/1     Running   0          2d\n",
);

const ALERTS: &str = concat!(
    "Alertname                    Starts At                  Summary\n",
    "CheckoutHighLatency          2026-11-02 08:12:04 UTC    p99 latency of POST /api/checkout above 1s for 10m\n",
    "KubeDeploymentRolloutStuck   2026-11-02 09:02:37 UTC    Deployment shop/shipping has not progressed for 15m\n",
    "NodeFilesystemAlmostFull     2026-11-01 22:40:13 UTC    ip-10-42-3-11 filesystem /var at 81%\n",
    "Watchdog                     2026-10-30 00:00:00 UTC    Alerting pipeline is working\n",
);

const EXPLAIN: &str = concat!(
    "                                                 QUERY PLAN\n",
    "------------------------------------------------------------------------------------------------------------\n",
    " Seq Scan on orders  (cost=0.00..48911.40 rows=37 width=219) (actual time=0.027..386.402 rows=41 loops=1)\n",
    "   Filter: (customer_id = 82113)\n",
    "   Rows Removed by Filter: 1993112\n",
    " Planning Time: 0.091 ms\n",
    " Execution Time: 386.457 ms\n",
    "(5 rows)\n",
);

const LATENCY_AFTER: &str = concat!(
    "ENDPOINT                   COUNT     P50      P95      P99\n",
    "GET /api/cart              17950     19ms     58ms     91ms\n",
    "POST /api/cart/items        6102     27ms     80ms    131ms\n",
    "POST /api/checkout          2244     96ms    207ms    310ms\n",
    "GET /api/shipping/quote     4387     31ms     69ms    114ms\n",
    "GET /actuator/health        5400      2ms      5ms      9ms\n",
);

const INCIDENT: &str = concat!(
    "ID:        INC-4821\n",
    "Title:     Checkout errors and latency in shop\n",
    "Severity:  SEV-2\n",
    "Owner:     payments-core\n",
    "Opened:    2026-11-02 08:15 UTC\n",
    "Status:    Mitigated\n",
    "Timeline:  9 entries\n",
);

const TOP: &str = concat!(
    "NAME                        CPU(cores)   MEMORY(bytes)\n",
    "checkout-api-8d4b2f-r8m2v   412m         1102Mi\n",
    "checkout-api-8d4b2f-t5pjq   398m         1088Mi\n",
    "checkout-api-8d4b2f-w3nbx   405m         1121Mi\n",
);

/// How one round's summary went, and what the summary in context states afterwards.
pub(crate) struct RoundScore {
    pub outcome: Outcome,
    pub error: Option<String>,
    /// Bytes of the summary request's input, rebuilt from the session; 0 when none was sent.
    pub request_bytes: usize,
    pub usage: Usage,
    /// Facts in the summary the model sees.
    pub facts: usize,
    /// Planted facts that hold after this round.
    pub expected: usize,
    /// Of those, the ones that a fact of the summary states.
    pub kept: usize,
    /// Replaced planted facts that the summary gives only the old value of: a fact states
    /// one, and none states its replacement.
    pub stale: usize,
    /// Replaced planted facts that a fact states without stating the replacement too, an
    /// account of what happened earlier included; another fact may state the replacement.
    pub old_values: usize,
}

/// The scripted session, summarized after each round.
pub(crate) struct Evaluation {
    pub rounds: Vec<RoundScore>,
    /// The facts of the summary in context at the end.
    pub facts: Vec<String>,
}

impl Evaluation {
    /// Print each round and the final facts, visible with `--nocapture`.
    pub(crate) fn print(&self, model: &str) {
        let mut total = Usage::default();
        for (index, round) in self.rounds.iter().enumerate() {
            total.add(&round.usage);
            eprintln!(
                "[summary eval] {model} round {}: {:?}, request {} bytes, {} tokens in / {} out, {} facts, kept {}/{}, stale {}, old values {}",
                index + 1,
                round.outcome,
                round.request_bytes,
                round.usage.input,
                round.usage.output,
                round.facts,
                round.kept,
                round.expected,
                round.stale,
                round.old_values
            );
            if let Some(error) = &round.error {
                eprintln!("[summary eval]   error: {error}");
            }
        }
        eprintln!(
            "[summary eval] {model}: {} tokens in / {} out over {} summaries; the final facts:",
            total.input,
            total.output,
            self.rounds.len()
        );
        for fact in &self.facts {
            eprintln!("[summary eval]   - {fact}");
        }
    }
}

/// Write the scripted session in `dir` and, after each round, summarize it with `provider`
/// and `model` under the budgets of `budget`; score every summary. Only the user's
/// next request stays out of a summary, whatever `budget` keeps. Panics if the session
/// cannot be written or does not load again at the end.
pub(crate) async fn evaluate(provider: Arc<dyn Provider>, model: &str, budget: Config, dir: &Path) -> Evaluation {
    let path = dir.join("summaries.jsonl");
    let mut store = SessionStore::create(&path, header()).unwrap();
    store.append_message(Message::user(TASK)).await.unwrap();
    drop(store);
    let mut rounds = Vec::new();
    let mut facts = Vec::new();
    for n in 1..=ROUNDS {
        let mut store = SessionStore::open(&path).unwrap();
        for message in round(n) {
            store.append_message(message).await.unwrap();
        }
        let sent_before = summary_requests(&store).len();
        let mut loop_config = config(Arc::clone(&provider), vec![]);
        loop_config.model = model.into();
        let (runner, _) = RunnerSetup {
            session: store,
            ..RunnerSetup::new(loop_config)
        }
        .build();
        let mut runner = runner
            .with_compaction(Config {
                // Only the user's next request stays as it is; the summary covers the rest.
                keep_recent_bytes: 1,
                ..budget
            })
            .unwrap();

        let report = runner.compact(&CancellationToken::new()).await;

        let store = runner.session();
        let request_bytes = match summary_requests(store).get(sent_before) {
            Some(id) => input_bytes(store, id),
            None => 0,
        };
        let summary = store.entries().iter().rev().find_map(|entry| match entry {
            Entry::Compaction(done) => Some(done),
            _ => None,
        });
        facts = summary.map_or_else(Vec::new, |done| {
            done.summary.facts.iter().map(|fact| fact.text.clone()).collect()
        });
        rounds.push(RoundScore {
            outcome: report.outcome,
            error: report.error,
            request_bytes,
            usage: report.usage,
            facts: facts.len(),
            expected: PLANTED.iter().filter(|fact| fact.holds_after(n)).count(),
            kept: PLANTED
                .iter()
                .filter(|fact| fact.holds_after(n) && facts.iter().any(|text| fact.stated_in(text)))
                .count(),
            stale: PLANTED.iter().filter(|fact| fact.stale_in(n, &facts)).count(),
            old_values: PLANTED.iter().filter(|fact| fact.old_value_in(n, &facts)).count(),
        });
    }
    // The session, the model's answers included, passes every check when loaded again.
    SessionStore::read(&path).unwrap();
    Evaluation { rounds, facts }
}

/// IDs of the summary requests recorded in `store`, oldest first.
fn summary_requests(store: &SessionStore) -> Vec<&str> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) if matches!(started.source, RequestSource::Summary { .. }) => {
                Some(started.id.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Bytes of a summary request's input, rebuilt from the session as an audit would.
fn input_bytes(store: &SessionStore, request_id: &str) -> usize {
    let (_, request) = store.replay_request(request_id).unwrap();
    match &request.messages[..] {
        [Message::System(_), Message::User { content, .. }] => content.len(),
        other => panic!("a summary request holds its instructions and its input, not {other:?}"),
    }
}
