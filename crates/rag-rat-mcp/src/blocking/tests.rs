use std::sync::Arc;
use std::time::Duration;

use rmcp::ErrorData;
use rmcp::model::CallToolResult;
use tokio::sync::Semaphore;

use super::{ToolTimeoutPolicy, parse_tool_timeout, parse_tool_workers, run_blocking_tool};

fn ok_result() -> CallToolResult {
    CallToolResult::success(vec![])
}

#[test]
fn tool_timeout_policy_classifies_write_tools() {
    assert_eq!(ToolTimeoutPolicy::for_tool("semantic_search"), ToolTimeoutPolicy::ReturnTimeout);
    assert_eq!(ToolTimeoutPolicy::for_tool("memory_create"), ToolTimeoutPolicy::WaitForCompletion);
}

#[test]
fn parse_helpers_reject_non_positive_values() {
    assert_eq!(parse_tool_timeout("0"), None);
    assert_eq!(parse_tool_workers("0"), None);
    assert_eq!(parse_tool_timeout("abc"), None);
    assert_eq!(parse_tool_workers("abc"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_blocking_tool_times_out_immediately_with_zero_budget() {
    let err = run_blocking_tool(
        "fast_tool".to_string(),
        Duration::ZERO,
        ToolTimeoutPolicy::ReturnTimeout,
        Arc::new(Semaphore::new(1)),
        || {
            std::thread::sleep(Duration::from_millis(50));
            Ok(ok_result())
        },
    )
    .await
    .expect_err("zero timeout must fail before blocking work runs");

    assert!(err.message.contains("timed out"), "got: {}", err.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_blocking_tool_propagates_runner_errors() {
    let err = run_blocking_tool(
        "failing_tool".to_string(),
        Duration::from_secs(1),
        ToolTimeoutPolicy::ReturnTimeout,
        Arc::new(Semaphore::new(1)),
        || Err(ErrorData::internal_error("runner failed".to_string(), None)),
    )
    .await
    .expect_err("runner errors must surface to the caller");

    assert!(err.message.contains("runner failed"), "got: {}", err.message);
}
