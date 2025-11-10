//! End-to-end samples: start here to learn the API by example.
//!
//! Each test demonstrates a common orchestration pattern using
//! `OrchestrationContext` and the in-process `Runtime`.
use duroxide::runtime::registry::ActivityRegistry;
use duroxide::runtime::{self};
use duroxide::{ActivityContext, Client, OrchestrationContext, OrchestrationRegistry};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::Once;
use tracing_subscriber::EnvFilter;

mod common;

static INIT_LOGGING: Once = Once::new();

fn init_test_logging() {
    INIT_LOGGING.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
        // Try to initialize, but ignore if already initialized (e.g., by duroxide or previous test run)
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
    });
}

/// Hello World: define one activity and call it from an orchestrator.
///
/// Highlights:
/// - Register an activity in an `ActivityRegistry`
/// - Start the `Runtime` with a provider (PostgreSQL here)
/// - Schedule an activity and await its typed completion
#[tokio::test]
async fn sample_hello_world_fs() {
    init_test_logging();
    let (store, schema_name) = common::create_postgres_store().await;

    // Register a simple activity: "Hello" -> format a greeting
    let activity_registry = ActivityRegistry::builder()
        .register("Hello", |ctx: ActivityContext, input: String| async move {
            ctx.trace_info("Hello activity started");
            let greeting = format!("Hello, {input}!");
            ctx.trace_info(format!("Hello activity completed -> {greeting}"));
            Ok(greeting)
        })
        .build();

    // Orchestrator: emit a trace, call Hello twice, return result using input
    let orchestration = |ctx: OrchestrationContext, input: String| async move {
        ctx.trace_info("hello_world started");
        let res = ctx
            .schedule_activity("Hello", "Rust")
            .into_activity()
            .await?;
        ctx.trace_info(format!("hello_world result={res} "));
        let res1 = ctx
            .schedule_activity("Hello", input)
            .into_activity()
            .await?;
        ctx.trace_info(format!("hello_world result={res1} "));
        Ok(res1)
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("HelloWorld", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sample-hello-1", "HelloWorld", "World")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sample-hello-1", std::time::Duration::from_secs(30))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "Hello, World!"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Basic control flow: branch on a flag returned by an activity.
///
/// Highlights:
/// - Call an activity to fetch a decision
/// - Use standard Rust control flow to drive subsequent activities
#[tokio::test]
async fn sample_basic_control_flow_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register activities that return a flag and branch outcomes
    let activity_registry = ActivityRegistry::builder()
        .register(
            "GetFlag",
            |_ctx: ActivityContext, _input: String| async move { Ok("yes".to_string()) },
        )
        .register("SayYes", |_ctx: ActivityContext, _in: String| async move {
            Ok("picked_yes".to_string())
        })
        .register("SayNo", |_ctx: ActivityContext, _in: String| async move {
            Ok("picked_no".to_string())
        })
        .build();

    // Orchestrator: get a flag and branch
    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        let flag = ctx
            .schedule_activity("GetFlag", "")
            .into_activity()
            .await
            .unwrap();
        ctx.trace_info(format!("control_flow flag decided = {flag}"));
        if flag == "yes" {
            Ok(ctx
                .schedule_activity("SayYes", "")
                .into_activity()
                .await
                .unwrap())
        } else {
            Ok(ctx
                .schedule_activity("SayNo", "")
                .into_activity()
                .await
                .unwrap())
        }
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("ControlFlow", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sample-cflow-1", "ControlFlow", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sample-cflow-1", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "picked_yes"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Loops and accumulation: call an activity repeatedly and build up a value.
///
/// Highlights:
/// - Use a for-loop in the orchestrator
/// - Emit replay-safe traces per iteration
#[tokio::test]
async fn sample_loop_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register an activity that appends "x" to its input
    let activity_registry = ActivityRegistry::builder()
        .register(
            "Append",
            |_ctx: ActivityContext, input: String| async move { Ok(format!("{input}x")) },
        )
        .build();

    // Orchestrator: loop three times, updating an accumulator
    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        let mut acc = String::from("start");
        for i in 0..3 {
            acc = ctx
                .schedule_activity("Append", acc)
                .into_activity()
                .await
                .unwrap();
            ctx.trace_info(format!("loop iteration {i} completed acc={acc}"));
        }
        Ok(acc)
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("LoopOrchestration", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sample-loop-1", "LoopOrchestration", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sample-loop-1", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "startxxx"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Error handling and compensation: recover from a failed activity.
///
/// Highlights:
/// - Activities return `Result<String, String>` and map into `Ok/Err`
/// - On failure, run a compensating activity and log what happened
#[tokio::test]
async fn sample_error_handling_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register a fragile activity that may fail, and a recovery activity
    let activity_registry = ActivityRegistry::builder()
        .register(
            "Fragile",
            |_ctx: ActivityContext, input: String| async move {
                if input == "bad" {
                    Err("boom".to_string())
                } else {
                    Ok("ok".to_string())
                }
            },
        )
        .register(
            "Recover",
            |_ctx: ActivityContext, _input: String| async move { Ok("recovered".to_string()) },
        )
        .build();

    // Orchestrator: try fragile, on error call Recover
    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        match ctx
            .schedule_activity("Fragile", "bad")
            .into_activity()
            .await
        {
            Ok(v) => {
                ctx.trace_info(format!("fragile succeeded value={v}"));
                Ok(v)
            }
            Err(e) => {
                ctx.trace_warn(format!("fragile failed error={e}"));
                let rec = ctx
                    .schedule_activity("Recover", "")
                    .into_activity()
                    .await
                    .unwrap();
                if rec != "recovered" {
                    ctx.trace_error(format!("unexpected recovery value={rec}"));
                }
                Ok(rec)
            }
        }
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("ErrorHandling", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sample-err-1", "ErrorHandling", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sample-err-1", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "recovered"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Timeouts via racing a long-running activity against a timer.
///
/// Highlights:
/// - Schedule a long-running activity and a short timer
/// - Use `ctx.select` to deterministically pick the earliest completion in history
/// - If the timer wins, return an error to the user
#[tokio::test]
async fn sample_timeout_with_timer_race_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register a long-running activity that sleeps before returning
    let activity_registry = ActivityRegistry::builder()
        .register(
            "LongOp",
            |ctx: ActivityContext, _input: String| async move {
                ctx.trace_info("LongOp started");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                ctx.trace_info("LongOp finished");
                Ok("done".to_string())
            },
        )
        .build();

    // Orchestration: race LongOp vs 100ms timer and error if timer wins
    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        let act = ctx.schedule_activity("LongOp", "");
        let t = ctx.schedule_timer(100);
        let (_idx, out) = ctx.select(vec![act, t]).await;
        match out {
            duroxide::DurableOutput::Timer => Err("timeout".to_string()),
            duroxide::DurableOutput::Activity(Ok(s)) => Ok(s),
            duroxide::DurableOutput::Activity(Err(e)) => Err(e),
            other => panic!("unexpected output: {other:?}"),
        }
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("TimeoutSample", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-timeout-sample", "TimeoutSample", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-timeout-sample", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Failed { details } => {
            assert_eq!(details.display_message(), "timeout")
        }
        runtime::OrchestrationStatus::Completed { output } => {
            panic!("expected timeout failure, got: {output}")
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Mixed race with select2: activity vs external event, demonstrate using the winner index.
///
/// Highlights:
/// - Schedule a slow activity and subscribe to an external event
/// - Use `ctx.select2(activity, external)` to pick the earliest completion
/// - Use the usize index from select2 to branch on which completed first
#[tokio::test]
async fn sample_select2_activity_vs_external_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register("Sleep", |ctx: ActivityContext, _input: String| async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            ctx.trace_info("Sleep activity finished");
            Ok("slept".to_string())
        })
        .build();

    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        let act = ctx.schedule_activity("Sleep", "");
        let evt = ctx.schedule_wait("Go");
        let (idx, out) = ctx.select2(act, evt).await;
        // Demonstrate using the index to branch
        match (idx, out) {
            (0, duroxide::DurableOutput::Activity(Ok(s))) => Ok(format!("activity:{s}")),
            (1, duroxide::DurableOutput::External(payload)) => Ok(format!("event:{payload}")),
            (0, duroxide::DurableOutput::Activity(Err(e))) => Err(e),
            other => panic!("unexpected: {other:?}"),
        }
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("Select2ActVsEvt", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;

    // Start orchestration, then raise external after subscription is recorded
    let store_for_wait = store.clone();
    tokio::spawn(async move {
        let sfw = store_for_wait.clone();
        let _ = common::wait_for_subscription(sfw.clone(), "inst-s2-mixed", "Go", 1000).await;
        let client = Client::new(sfw);
        let _ = client.raise_event("inst-s2-mixed", "Go", "ok").await;
    });
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-s2-mixed", "Select2ActVsEvt", "")
        .await
        .unwrap();

    let s = match client
        .wait_for_orchestration("inst-s2-mixed", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => output,
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    };
    // External event should win (idx==1) because activity sleeps 300ms
    assert_eq!(s, "event:ok");
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Parallel fan-out/fan-in: run two activities concurrently and join results.
///
/// Highlights:
/// - Use `ctx.join` to await multiple `DurableFuture`s concurrently in history order
/// - Deterministic replay ensures join order follows history
#[tokio::test]
async fn dtf_legacy_gabbar_greetings_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register a greeting activity used by both branches
    let activity_registry = ActivityRegistry::builder()
        .register(
            "Greetings",
            |ctx: ActivityContext, input: String| async move {
                ctx.trace_info("Greeting activity started");
                ctx.trace_debug(format!("Original input: {input}"));
                let output = format!("Hello, {input}!");
                ctx.trace_info(format!("Greeting activity completed -> {output}"));
                Ok(output)
            },
        )
        .build();

    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        // Schedule two greetings in parallel using deterministic join
        let a = ctx.schedule_activity("Greetings", "Gabbar");
        let b = ctx.schedule_activity("Greetings", "Samba");
        let outs = ctx.join(vec![a, b]).await;
        let mut vals: Vec<String> = outs
            .into_iter()
            .map(|o| match o {
                duroxide::DurableOutput::Activity(Ok(s)) => s,
                duroxide::DurableOutput::Activity(Err(e)) => panic!("activity failed: {e}"),
                other => panic!("unexpected output: {other:?}"),
            })
            .collect();
        // For a stable assertion build a canonical order
        vals.sort();
        Ok(format!("{}, {}", vals[0].clone(), vals[1].clone()))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("Greetings", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-dtf-greetings", "Greetings", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-dtf-greetings", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => {
            assert_eq!(output, "Hello, Gabbar!, Hello, Samba!")
        }
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// System activities: use built-in activities to get wall-clock time and a new GUID.
///
/// Highlights:
/// - Call `ctx.system_now_ms()` and `ctx.system_new_guid()`
/// - Log and validate basic formatting of results
#[tokio::test]
async fn sample_system_activities_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder().build();

    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        let now = ctx.utcnow_ms().await?;
        let guid = ctx.new_guid().await?;
        ctx.trace_info(format!("system now={now}, guid={guid}"));
        Ok(format!("n={now},g={guid}"))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("SystemActivities", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-system-acts", "SystemActivities", "")
        .await
        .unwrap();

    let out = match client
        .wait_for_orchestration("inst-system-acts", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => output,
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    };
    // Basic assertions
    assert!(out.contains("n=") && out.contains(",g="));
    let parts: Vec<&str> = out.split([',', '=']).collect();
    // parts like ["n", now, "g", guid]
    assert!(parts.len() >= 4);
    let now_val: u64 = parts[1].parse().unwrap_or(0);
    let guid_str = parts[3];
    assert!(now_val > 0);
    // GUID format: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" (36 chars with hyphens)
    assert_eq!(guid_str.len(), 36);
    assert!(guid_str
        .chars()
        .filter(|c| *c != '-')
        .all(|c| c.is_ascii_hexdigit()));

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Sample: start an orchestration and poll its status until completion.
#[tokio::test]
async fn sample_status_polling_fs() {
    use duroxide::OrchestrationStatus;
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder().build();
    let orchestration = |ctx: OrchestrationContext, _input: String| async move {
        ctx.schedule_timer(20).into_timer().await;
        Ok("done".to_string())
    };
    let orchestration_registry = OrchestrationRegistry::builder()
        .register("StatusSample", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-status-sample", "StatusSample", "")
        .await
        .unwrap();

    // New helper: wait until terminal (Completed/Failed) or timeout.
    match client
        .wait_for_orchestration("inst-status-sample", std::time::Duration::from_secs(2))
        .await
        .unwrap()
    {
        OrchestrationStatus::Completed { output } => assert_eq!(output, "done"),
        OrchestrationStatus::Failed { details } => {
            panic!("unexpected failure: {}", details.display_message())
        }
        _ => unreachable!(),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Sub-orchestrations: simple parent/child orchestration.
///
/// Highlights:
/// - Parent calls a child orchestration and awaits its result
/// - Child uses an activity and returns its output
#[tokio::test]
async fn sample_sub_orchestration_basic_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register("Upper", |ctx: ActivityContext, input: String| async move {
            ctx.trace_info("Upper activity converting string");
            let result = input.to_uppercase();
            ctx.trace_info(format!("Upper activity result -> {result}"));
            Ok(result)
        })
        .build();

    let child_upper = |ctx: OrchestrationContext, input: String| async move {
        let up = ctx
            .schedule_activity("Upper", input)
            .into_activity()
            .await
            .unwrap();
        Ok(up)
    };
    let parent = |ctx: OrchestrationContext, input: String| async move {
        let r = ctx
            .schedule_sub_orchestration("ChildUpper", input)
            .into_sub_orchestration()
            .await
            .unwrap();
        Ok(format!("parent:{r}"))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("ChildUpper", child_upper)
        .register("Parent", parent)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sub-basic", "Parent", "hi")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sub-basic", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "parent:HI"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Sub-orchestrations: fan-out to multiple children and join.
///
/// Highlights:
/// - Parent starts two child orchestrations in parallel
/// - Uses `ctx.join` to await both in history order and aggregates results
#[tokio::test]
async fn sample_sub_orchestration_fanout_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register("Add", |_ctx: ActivityContext, input: String| async move {
            let mut it = input.split(',');
            let a = it.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
            let b = it.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
            Ok((a + b).to_string())
        })
        .build();

    let child_sum = |ctx: OrchestrationContext, input: String| async move {
        let s = ctx
            .schedule_activity("Add", input)
            .into_activity()
            .await
            .unwrap();
        Ok(s)
    };
    let parent = |ctx: OrchestrationContext, _input: String| async move {
        let a = ctx.schedule_sub_orchestration("ChildSum", "1,2");
        let b = ctx.schedule_sub_orchestration("ChildSum", "3,4");
        let outs = ctx.join(vec![a, b]).await;
        let mut nums: Vec<i64> = outs
            .into_iter()
            .map(|o| match o {
                duroxide::DurableOutput::SubOrchestration(Ok(s)) => s.parse::<i64>().unwrap(),
                duroxide::DurableOutput::SubOrchestration(Err(e)) => panic!("child failed: {e}"),
                other => panic!("unexpected output: {other:?}"),
            })
            .collect();
        let total: i64 = nums.drain(..).sum();
        Ok(format!("total={total}"))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("ChildSum", child_sum)
        .register("ParentFan", parent)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sub-fan", "ParentFan", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sub-fan", std::time::Duration::from_secs(10))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "total=10"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Sub-orchestrations: chained (root -> mid -> leaf).
///
/// Highlights:
/// - Root calls Mid; Mid calls Leaf; each returns a transformed value
/// - Demonstrates nested sub-orchestrations
#[tokio::test]
async fn sample_sub_orchestration_chained_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register(
            "AppendX",
            |_ctx: ActivityContext, input: String| async move { Ok(format!("{input}x")) },
        )
        .build();

    let leaf = |ctx: OrchestrationContext, input: String| async move {
        Ok(ctx
            .schedule_activity("AppendX", input)
            .into_activity()
            .await
            .unwrap())
    };
    let mid = |ctx: OrchestrationContext, input: String| async move {
        let r = ctx
            .schedule_sub_orchestration("Leaf", input)
            .into_sub_orchestration()
            .await
            .unwrap();
        Ok(format!("{r}-mid"))
    };
    let root = |ctx: OrchestrationContext, input: String| async move {
        let r = ctx
            .schedule_sub_orchestration("Mid", input)
            .into_sub_orchestration()
            .await
            .unwrap();
        Ok(format!("root:{r}"))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("Leaf", leaf)
        .register("Mid", mid)
        .register("Root", root)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sub-chain", "Root", "a")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sub-chain", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "root:ax-mid"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Detached orchestration scheduling: start independent orchestrations without awaiting.
///
/// Highlights:
/// - Use `ctx.schedule_orchestration(name, instance, input)` with explicit instance IDs
/// - No parent/child semantics; scheduled orchestrations are independent roots
/// - Verify scheduled instances complete via status polling
#[tokio::test]
async fn sample_detached_orchestration_scheduling_fs() {
    use duroxide::OrchestrationStatus;
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register("Echo", |_ctx: ActivityContext, input: String| async move {
            Ok(input)
        })
        .build();

    let chained = |ctx: OrchestrationContext, input: String| async move {
        ctx.schedule_timer(5).into_timer().await;
        Ok(ctx
            .schedule_activity("Echo", input)
            .into_activity()
            .await
            .unwrap())
    };
    let coordinator = |ctx: OrchestrationContext, _input: String| async move {
        ctx.schedule_orchestration("Chained", "W1", "A");
        ctx.schedule_orchestration("Chained", "W2", "B");
        Ok("scheduled".to_string())
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("Chained", chained)
        .register("Coordinator", coordinator)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("CoordinatorRoot", "Coordinator", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("CoordinatorRoot", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "scheduled"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    // The scheduled instances are plain W1/W2 (no prefixing)
    let insts = vec!["W1".to_string(), "W2".to_string()];
    for inst in insts {
        match client
            .wait_for_orchestration(&inst, std::time::Duration::from_secs(5))
            .await
            .unwrap()
        {
            OrchestrationStatus::Completed { output } => {
                assert!(output == "A" || output == "B");
            }
            OrchestrationStatus::Failed { details } => {
                panic!(
                    "scheduled orchestration failed: {}",
                    details.display_message()
                )
            }
            _ => unreachable!(),
        }
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// ContinueAsNew sample: roll over input across executions until a condition is met.
///
/// Highlights:
/// - Use `ctx.continue_as_new(new_input)` to terminate current execution and start a new one
/// - Provider keeps all execution histories; latest execution holds the final result
#[tokio::test]
async fn sample_continue_as_new_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder().build();
    let orch = |ctx: OrchestrationContext, input: String| async move {
        let n: u32 = input.parse().unwrap_or(0);
        if n < 3 {
            ctx.trace_info(format!("CAN sample n={n} -> continue"));
            ctx.continue_as_new((n + 1).to_string());
            Ok(String::new())
        } else {
            Ok(format!("final:{n}"))
        }
    };
    let orchestration_registry = OrchestrationRegistry::builder()
        .register("CanSample", orch)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sample-can", "CanSample", "0")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sample-can", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "final:3"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }
    // Check executions exist
    let admin = store
        .as_management_capability()
        .expect("Management capability should be available");
    let execs = admin
        .list_executions("inst-sample-can")
        .await
        .expect("list_executions should succeed");
    assert_eq!(execs, vec![1, 2, 3, 4]);
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

// Typed samples

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AddReq {
    a: i32,
    b: i32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AddRes {
    sum: i32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Ack {
    ok: bool,
}

/// Typed activity + typed orchestration: Add two numbers and return a struct
#[tokio::test]
async fn sample_typed_activity_and_orchestration_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register_typed::<AddReq, AddRes, _, _>("Add", |_ctx: ActivityContext, req| async move {
            Ok(AddRes { sum: req.a + req.b })
        })
        .build();

    let orchestration = |ctx: OrchestrationContext, req: AddReq| async move {
        let out: AddRes = ctx
            .schedule_activity_typed::<AddReq, AddRes>("Add", &req)
            .into_activity_typed::<AddRes>()
            .await?;
        Ok(out)
    };
    let orchestration_registry = OrchestrationRegistry::builder()
        .register_typed::<AddReq, AddRes, _, _>("Adder", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration_typed::<AddReq>("inst-typed-add", "Adder", AddReq { a: 2, b: 3 })
        .await
        .unwrap();

    match client
        .wait_for_orchestration_typed::<AddRes>("inst-typed-add", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        Ok(result) => assert_eq!(result, AddRes { sum: 5 }),
        Err(error) => panic!("orchestration failed: {error}"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Typed external event sample: await Ack { ok } from an event
#[tokio::test]
async fn sample_typed_event_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder().build();
    let orch = |ctx: OrchestrationContext, _in: ()| async move {
        let ack: Ack = ctx
            .schedule_wait_typed::<Ack>("Ready")
            .into_event_typed::<Ack>()
            .await;
        Ok::<_, String>(serde_json::to_string(&ack).unwrap())
    };
    let orchestration_registry = OrchestrationRegistry::builder()
        .register_typed::<(), String, _, _>("WaitAck", orch)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let store_for_wait = store.clone();
    tokio::spawn(async move {
        let sfw = store_for_wait.clone();
        let _ = common::wait_for_subscription(sfw.clone(), "inst-typed-ack", "Ready", 1000).await;
        // Raise typed event by serializing payload
        let payload = serde_json::to_string(&Ack { ok: true }).unwrap();
        let client = Client::new(sfw);
        let _ = client.raise_event("inst-typed-ack", "Ready", payload).await;
    });
    let client = Client::new(store.clone());
    client
        .start_orchestration_typed::<()>("inst-typed-ack", "WaitAck", ())
        .await
        .unwrap();

    match client
        .wait_for_orchestration_typed::<String>("inst-typed-ack", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        Ok(result) => assert_eq!(result, serde_json::to_string(&Ack { ok: true }).unwrap()),
        Err(error) => panic!("orchestration failed: {error}"),
    }
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Mixed string and typed activities with typed orchestration, showcasing select on typed+string
#[tokio::test]
async fn sample_mixed_string_and_typed_typed_orch_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // String activity: returns uppercased string
    // Typed activity: Add two numbers
    let activity_registry = ActivityRegistry::builder()
        .register("Upper", |_ctx: ActivityContext, input: String| async move {
            Ok(input.to_uppercase())
        })
        .register_typed::<AddReq, AddRes, _, _>("Add", |_ctx: ActivityContext, req| async move {
            Ok(AddRes { sum: req.a + req.b })
        })
        .build();

    // Typed orchestrator input/output
    let orch = |ctx: OrchestrationContext, req: AddReq| async move {
        // Kick off a typed activity and a string activity, race them with deterministic select
        let f_typed = ctx.schedule_activity_typed::<AddReq, AddRes>("Add", &req);
        let f_str = ctx.schedule_activity("Upper", "hello");
        let (_idx, out) = ctx.select(vec![f_typed, f_str]).await;
        let s = match out {
            duroxide::DurableOutput::Activity(Ok(raw)) => {
                // raw is either typed AddRes JSON or plain string result
                if let Ok(v) = serde_json::from_str::<AddRes>(&raw) {
                    format!("sum={}", v.sum)
                } else {
                    format!("up={raw}")
                }
            }
            duroxide::DurableOutput::Activity(Err(e)) => return Err(e),
            other => panic!("unexpected output: {other:?}"),
        };
        Ok::<_, String>(s)
    };
    let orchestration_registry = OrchestrationRegistry::builder()
        .register_typed::<AddReq, String, _, _>("MixedTypedOrch", orch)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration_typed::<AddReq>(
            "inst-mixed-typed",
            "MixedTypedOrch",
            AddReq { a: 1, b: 2 },
        )
        .await
        .unwrap();
    let client = Client::new(store.clone());

    let s = match client
        .wait_for_orchestration_typed::<String>(
            "inst-mixed-typed",
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap()
    {
        Ok(result) => result,
        Err(error) => panic!("orchestration failed: {error}"),
    };
    assert!(s == "sum=3" || s == "up=HELLO");
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Mixed string and typed activities with string orchestration, showcasing select on typed+string
#[tokio::test]
async fn sample_mixed_string_and_typed_string_orch_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let activity_registry = ActivityRegistry::builder()
        .register("Upper", |_ctx: ActivityContext, input: String| async move {
            Ok(input.to_uppercase())
        })
        .register_typed::<AddReq, AddRes, _, _>("Add", |_ctx: ActivityContext, req| async move {
            Ok(AddRes { sum: req.a + req.b })
        })
        .build();

    // String orchestrator mixes typed and string activity calls
    let orch = |ctx: OrchestrationContext, _in: String| async move {
        let f_typed = ctx.schedule_activity_typed::<AddReq, AddRes>("Add", &AddReq { a: 5, b: 7 });
        let f_str = ctx.schedule_activity("Upper", "race");
        let (_idx, out) = ctx.select(vec![f_typed, f_str]).await;
        let s = match out {
            duroxide::DurableOutput::Activity(Ok(raw)) => {
                if let Ok(v) = serde_json::from_str::<AddRes>(&raw) {
                    format!("sum={}", v.sum)
                } else {
                    format!("up={raw}")
                }
            }
            duroxide::DurableOutput::Activity(Err(e)) => return Err(e),
            other => panic!("unexpected output: {other:?}"),
        };
        Ok::<_, String>(s)
    };
    let orch_reg = OrchestrationRegistry::builder()
        .register("MixedStringOrch", orch)
        .build();

    let rt =
        runtime::Runtime::start_with_store(store.clone(), Arc::new(activity_registry), orch_reg)
            .await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-mixed-string", "MixedStringOrch", "")
        .await
        .unwrap();

    let s = match client
        .wait_for_orchestration("inst-mixed-string", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => output,
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    };
    assert!(s == "sum=12" || s == "up=RACE");
    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Versioning: default latest vs pinned exact on start
///
/// Highlights:
/// - Register two versions of the same orchestration using semver (1.0.0 and 2.0.0)
/// - Default policy (Latest) picks the highest on new starts
/// - Changing policy to Exact pins new starts to a specific version
#[tokio::test]
async fn sample_versioning_start_latest_vs_exact_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Two versions: return a string indicating which version executed
    let v1 = |_: OrchestrationContext, _in: String| async move { Ok("v1".to_string()) };
    let v2 = |_: OrchestrationContext, _in: String| async move { Ok("v2".to_string()) };

    let reg = OrchestrationRegistry::builder()
        // Default registration is 1.0.0
        .register("Versioned", v1)
        // Add a later version 2.0.0
        .register_versioned("Versioned", "2.0.0", v2)
        .build();
    let acts = ActivityRegistry::builder().build();
    let rt = runtime::Runtime::start_with_store(store.clone(), Arc::new(acts), reg.clone()).await;

    // With default policy (Latest), a new start should run v2
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-vers-latest", "Versioned", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-vers-latest", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "v2"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    // Pin new starts to 1.0.0 via policy, verify it runs v1
    reg.set_version_policy(
        "Versioned",
        duroxide::runtime::VersionPolicy::Exact(semver::Version::parse("1.0.0").unwrap()),
    )
    .await;
    client
        .start_orchestration("inst-vers-exact", "Versioned", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-vers-exact", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "v1"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Versioning: sub-orchestration explicit version vs default policy
///
/// Highlights:
/// - Parent calls child once with an explicit version and once without
/// - The explicit call uses 1.0.0; the policy (Latest) uses 2.0.0
#[tokio::test]
async fn sample_versioning_sub_orchestration_explicit_vs_policy_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    let child_v1 = |_: OrchestrationContext, _in: String| async move { Ok("c1".to_string()) };
    let child_v2 = |_: OrchestrationContext, _in: String| async move { Ok("c2".to_string()) };
    let parent = |ctx: OrchestrationContext, _in: String| async move {
        // Explicit versioned call -> expect c1
        let a = ctx
            .schedule_sub_orchestration_versioned("Child", Some("1.0.0".to_string()), "exp")
            .into_sub_orchestration()
            .await
            .unwrap();
        // Policy-based call (Latest) -> expect c2
        let b = ctx
            .schedule_sub_orchestration("Child", "pol")
            .into_sub_orchestration()
            .await
            .unwrap();
        Ok(format!("{a}-{b}"))
    };

    let reg = OrchestrationRegistry::builder()
        .register("ParentVers", parent)
        .register("Child", child_v1)
        .register_versioned("Child", "2.0.0", child_v2)
        .build();
    let acts = ActivityRegistry::builder().build();
    let rt = runtime::Runtime::start_with_store(store.clone(), Arc::new(acts), reg).await;
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sub-vers", "ParentVers", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-sub-vers", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => assert_eq!(output, "c1-c2"),
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Versioning + ContinueAsNew: safe upgrade of a long-running (infinite) orchestration
///
/// Highlights:
/// - Use `continue_as_new(new_input)` to roll to a fresh execution that picks the default version
///   from the registry policy (Latest by default, or a pinned Exact if set)
/// - Avoids nondeterminism because the new execution starts fresh at the version boundary
/// - Carry forward state via the CAN input, or transform as needed during upgrade
#[tokio::test]
async fn sample_versioning_continue_as_new_upgrade_fs() {
    use duroxide::OrchestrationStatus;
    let (store, schema_name) = common::create_postgres_store().await;

    // v1: simulate deciding to upgrade at a maintenance boundary (e.g., at the end of a cycle)
    // In a real infinite loop, you'd do some work (timer/activity), then CAN to v2.
    let v1 = |ctx: OrchestrationContext, input: String| async move {
        ctx.trace_info("v1: upgrading via ContinueAsNew (default policy)".to_string());
        // Roll to a fresh execution, marking the payload so we can attribute it to v1 deterministically
        ctx.continue_as_new(format!("v1:{input}"));
        Ok(String::new())
    };
    // v2: represents the upgraded logic. Here we just simulate one step and complete for the sample.
    let v2 = |ctx: OrchestrationContext, input: String| async move {
        ctx.trace_info(format!("v2: resumed with input={input}"));
        Ok(format!("upgraded:{input}"))
    };

    let reg = OrchestrationRegistry::builder()
        .register("LongRunner", v1) // implicit 1.0.0
        .register_versioned("LongRunner", "2.0.0", v2)
        .build();
    let acts = ActivityRegistry::builder().build();
    let rt = runtime::Runtime::start_with_store(store.clone(), Arc::new(acts), reg).await;

    // Start on v1; the first handle will resolve at the CAN boundary
    // Pin initial start to v1 explicitly to demonstrate upgrade via CAN; default policy remains Latest (v2)
    let client = Client::new(store.clone());
    client
        .start_orchestration_versioned("inst-can-upgrade", "LongRunner", "1.0.0", "state")
        .await
        .unwrap();

    // Poll for the new execution (v2) to complete
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match client.get_orchestration_status("inst-can-upgrade").await {
            OrchestrationStatus::Completed { output } => {
                assert_eq!(output, "upgraded:v1:state");
                break;
            }
            OrchestrationStatus::Failed { details } => {
                panic!("unexpected failure: {}", details.display_message())
            }
            _ if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await
            }
            _ => panic!("timeout waiting for upgraded completion"),
        }
    }

    // Verify two executions exist, exec1 continued-as-new, exec2 completed with v2 output
    let admin = store
        .as_management_capability()
        .expect("Management capability should be available");
    let execs = admin
        .list_executions("inst-can-upgrade")
        .await
        .expect("list_executions should succeed");
    assert_eq!(execs, vec![1, 2]);
    let e1 = store
        .read_with_execution("inst-can-upgrade", 1)
        .await
        .expect("read_with_execution should succeed");
    assert!(e1
        .iter()
        .any(|e| matches!(e, duroxide::Event::OrchestrationContinuedAsNew { .. })));
    // Exec2 must start with the v1-marked payload, proving v1 ran first and handed off via CAN
    let e2 = store
        .read_with_execution("inst-can-upgrade", 2)
        .await
        .expect("read_with_execution should succeed");
    assert!(e2.iter().any(
        |e| matches!(e, duroxide::Event::OrchestrationStarted { input, .. } if input == "v1:state")
    ));

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Cancellation: cancel a parent orchestration and observe cascading cancellation to children.
///
/// Highlights:
/// - Parent starts a child and awaits it
/// - We cancel the parent instance via the runtime API
/// - The parent fails deterministically with a canonical "canceled: <reason>"
/// - The child is also canceled (downward propagation), and its history shows cancellation
#[tokio::test]
async fn sample_cancellation_parent_cascades_to_children_fs() {
    use duroxide::Event;
    let (store, schema_name) = common::create_postgres_store().await;

    // Child: waits forever (until canceled). This demonstrates cooperative cancellation via runtime.
    let child = |ctx: OrchestrationContext, _input: String| async move {
        let _ = ctx.schedule_wait("Go").into_event().await;
        Ok("done".to_string())
    };

    // Parent: starts child and awaits its completion.
    let parent = |ctx: OrchestrationContext, _input: String| async move {
        let _ = ctx
            .schedule_sub_orchestration("ChildSample", "seed")
            .into_sub_orchestration()
            .await?;
        Ok::<_, String>("parent_done".to_string())
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("ChildSample", child)
        .register("ParentSample", parent)
        .build();
    let activity_registry = ActivityRegistry::builder().build();

    // Use faster polling for cancellation timing test
    let options = runtime::RuntimeOptions {
        dispatcher_idle_sleep_ms: 10,
        ..Default::default()
    };
    let rt = runtime::Runtime::start_with_options(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
        options,
    )
    .await;

    // Start the parent orchestration
    let client = Client::new(store.clone());
    client
        .start_orchestration("inst-sample-cancel", "ParentSample", "")
        .await
        .unwrap();

    // Allow scheduling turn to run and child to start
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Cancel the parent; the runtime will append OrchestrationCancelRequested and then OrchestrationFailed
    let _ = client
        .cancel_instance("inst-sample-cancel", "user_request")
        .await;

    // Wait for the parent to fail deterministically with a canceled error
    let ok = common::wait_for_history(
        store.clone(),
        "inst-sample-cancel",
        |hist| {
            hist.iter().rev().any(|e| {
                matches!(
                    e,
                    Event::OrchestrationFailed { details, .. } if matches!(
                        details,
                        duroxide::ErrorDetails::Application {
                            kind: duroxide::AppErrorKind::Cancelled { reason },
                            ..
                        } if reason == "user_request"
                    )
                )
            })
        },
        5_000,
    )
    .await;
    assert!(ok, "timeout waiting for parent cancel failure");

    // Find child instance (prefix is parent::sub::<id>) and check it was canceled too
    let admin = store
        .as_management_capability()
        .expect("Management capability should be available");
    let children: Vec<String> = admin
        .list_instances()
        .await
        .expect("list_instances should succeed")
        .into_iter()
        .filter(|i| i.starts_with("inst-sample-cancel::"))
        .collect();
    assert!(!children.is_empty());
    for child in children {
        let ok_child = common::wait_for_history(
            store.clone(),
            &child,
            |hist| {
                hist.iter()
                    .any(|e| matches!(e, Event::OrchestrationCancelRequested { .. }))
                    && hist.iter().any(|e| {
                        matches!(
                            e,
                            Event::OrchestrationFailed { details, .. } if matches!(
                                details,
                                duroxide::ErrorDetails::Application {
                                    kind: duroxide::AppErrorKind::Cancelled { reason },
                                    ..
                                } if reason == "parent canceled"
                            )
                        )
                    })
            },
            5_000,
        )
        .await;
        assert!(ok_child, "timeout waiting for child cancel for {child}");
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Error handling: basic activity failure
///
/// Highlights:
/// - Activity that can fail
/// - Error propagation from activity to orchestration
/// - Simple error handling pattern
#[tokio::test]
async fn sample_basic_error_handling_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register an activity that can fail
    let activity_registry = ActivityRegistry::builder()
        .register(
            "ValidateInput",
            |_ctx: ActivityContext, input: String| async move {
                if input.is_empty() {
                    Err("Input cannot be empty".to_string())
                } else {
                    Ok(format!("Valid: {input}"))
                }
            },
        )
        .build();

    // Simple orchestration that calls the activity
    let orchestration = |ctx: OrchestrationContext, input: String| async move {
        ctx.trace_info("Starting validation");
        let result = ctx
            .schedule_activity("ValidateInput", input)
            .into_activity()
            .await?;
        ctx.trace_info(format!("Validation result: {result}"));
        Ok(result)
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("BasicErrorHandling", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());

    // Test successful case
    client
        .start_orchestration("inst-basic-error-1", "BasicErrorHandling", "test")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-basic-error-1", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => {
            assert_eq!(output, "Valid: test");
        }
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    // Test error case
    client
        .start_orchestration("inst-basic-error-2", "BasicErrorHandling", "")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-basic-error-2", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Failed { details } => {
            assert!(details.display_message().contains("Input cannot be empty"));
        }
        runtime::OrchestrationStatus::Completed { output } => {
            panic!("Expected failure but got success: {output}")
        }
        _ => panic!("unexpected orchestration status"),
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Error handling: nested function with `?` operator
///
/// Highlights:
/// - Nested function that can fail
/// - Using `?` operator for error propagation
/// - Clean error handling pattern
#[tokio::test]
async fn sample_nested_function_error_handling_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register activities
    let activity_registry = ActivityRegistry::builder()
        .register(
            "ProcessData",
            |_ctx: ActivityContext, input: String| async move {
                if input.contains("error") {
                    Err("Processing failed".to_string())
                } else {
                    Ok(format!("Processed: {input}"))
                }
            },
        )
        .register(
            "FormatOutput",
            |_ctx: ActivityContext, input: String| async move { Ok(format!("Final: {input}")) },
        )
        .build();

    // Nested function that can fail with `?`
    async fn process_and_format(ctx: &OrchestrationContext, data: &str) -> Result<String, String> {
        ctx.trace_info("Starting processing");
        let processed = ctx
            .schedule_activity("ProcessData", data.to_string())
            .into_activity()
            .await?;
        ctx.trace_info("Starting formatting");
        let formatted = ctx
            .schedule_activity("FormatOutput", processed)
            .into_activity()
            .await?;
        Ok(formatted)
    }

    // Orchestration that uses nested function with `?`
    let orchestration = |ctx: OrchestrationContext, input: String| async move {
        ctx.trace_info("Starting orchestration");
        let result = process_and_format(&ctx, &input).await?;
        ctx.trace_info("Orchestration completed");
        Ok(result)
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("NestedErrorHandling", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());

    // Test successful case
    client
        .start_orchestration("inst-nested-error-1", "NestedErrorHandling", "test")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-nested-error-1", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => {
            assert_eq!(output, "Final: Processed: test");
        }
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    // Test error case
    client
        .start_orchestration("inst-nested-error-2", "NestedErrorHandling", "error")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-nested-error-2", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Failed { details } => {
            assert!(details.display_message().contains("Processing failed"));
        }
        runtime::OrchestrationStatus::Completed { output } => {
            panic!("Expected failure but got success: {output}")
        }
        _ => panic!("unexpected orchestration status"),
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}

/// Error handling: error recovery with logging
///
/// Highlights:
/// - Explicit error handling with match statements
/// - Error recovery and logging
/// - Graceful failure handling
#[tokio::test]
async fn sample_error_recovery_fs() {
    let (store, schema_name) = common::create_postgres_store().await;

    // Register activities
    let activity_registry = ActivityRegistry::builder()
        .register(
            "ProcessData",
            |_ctx: ActivityContext, input: String| async move {
                if input.contains("error") {
                    Err("Processing failed".to_string())
                } else {
                    Ok(format!("Processed: {input}"))
                }
            },
        )
        .register(
            "LogError",
            |_ctx: ActivityContext, error: String| async move { Ok(format!("Logged: {error}")) },
        )
        .build();

    // Orchestration with explicit error recovery
    let orchestration = |ctx: OrchestrationContext, input: String| async move {
        ctx.trace_info("Starting orchestration");

        match ctx
            .schedule_activity("ProcessData", input.clone())
            .into_activity()
            .await
        {
            Ok(result) => {
                ctx.trace_info("Processing succeeded");
                Ok(result)
            }
            Err(e) => {
                ctx.trace_info("Processing failed, logging error");
                let _ = ctx
                    .schedule_activity("LogError", e.clone())
                    .into_activity()
                    .await;
                Err(format!("Failed to process '{input}': {e}"))
            }
        }
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("ErrorRecovery", orchestration)
        .build();

    let rt = runtime::Runtime::start_with_store(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
    )
    .await;
    let client = Client::new(store.clone());

    // Test successful case
    client
        .start_orchestration("inst-recovery-1", "ErrorRecovery", "test")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-recovery-1", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Completed { output } => {
            assert_eq!(output, "Processed: test");
        }
        runtime::OrchestrationStatus::Failed { details } => {
            panic!("orchestration failed: {}", details.display_message())
        }
        _ => panic!("unexpected orchestration status"),
    }

    // Test error recovery case
    client
        .start_orchestration("inst-recovery-2", "ErrorRecovery", "error")
        .await
        .unwrap();

    match client
        .wait_for_orchestration("inst-recovery-2", std::time::Duration::from_secs(5))
        .await
        .unwrap()
    {
        runtime::OrchestrationStatus::Failed { details } => {
            let error_msg = details.display_message();
            assert!(error_msg.contains("Failed to process 'error'"));
            assert!(error_msg.contains("Processing failed"));
        }
        runtime::OrchestrationStatus::Completed { output } => {
            panic!("Expected failure but got success: {output}")
        }
        _ => panic!("unexpected orchestration status"),
    }

    rt.shutdown(None).await;
    common::cleanup_schema(&schema_name).await;
}
