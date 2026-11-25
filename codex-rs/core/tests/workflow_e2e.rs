//! E2E-01..E2E-10 for the workflow runtime (TASK-040).
//!
//! E2E-01 runs the real Rhai engine and the real host-service protocol with a
//! scripted mock child, so it needs no model credentials. The remaining cases
//! need a real model turn, Windows/MSVC, or a process-restart driver and are
//! kept here as ignored carriers; `evidence/E2E-F.md` records their status.

use serde_json::json;

use codex_core::workflow::testing::MockAgentReply;
use codex_core::workflow::testing::builtin_script;
use codex_core::workflow::testing::run_script_with_mock_agents;

#[tokio::test]
async fn e2e_01_hello_workflow_runs_end_to_end_with_a_mock_child() {
    let script = builtin_script("hello-workflow").expect("hello-workflow is compiled in");
    let report = run_script_with_mock_agents(
        script,
        json!({ "name": "yeax" }),
        vec![MockAgentReply::text("Hello, yeax!")],
    )
    .await
    .expect("engine run");

    assert!(
        report.completed,
        "hello-workflow must complete: {:?}",
        report.error
    );
    assert_eq!(report.output["ok"], json!(true));
    assert_eq!(report.output["greeting"], json!("Hello, yeax!"));
    assert_eq!(report.prompts.len(), 1, "exactly one child turn");
    assert!(
        report.prompts[0].contains("greeting for yeax"),
        "prompt must name the arg: {}",
        report.prompts[0]
    );
    let run_dir = report.run_dir.expect("run directory");
    assert!(run_dir.join("journal.jsonl").is_file(), "journal written");
}

#[tokio::test]
async fn e2e_02_deep_research_four_phases() {
    let script = builtin_script("deep-research").expect("deep-research is compiled in");
    let report = run_script_with_mock_agents(
        script,
        json!({ "query": "yeax workflow migration" }),
        vec![
            // Plan: one question keeps the remaining fan-out deterministic.
            MockAgentReply::json(json!({
                "questions": ["What did the workflow migration change?"],
            })),
            // Research: one question -> one candidate claim.
            MockAgentReply::json(json!({
                "claims": [{
                    "claim": "The workflow runtime moved into Yeax core.",
                    "evidence": "The migration patch adds core/src/workflow.",
                    "source_title": "yeax-workflow.patch",
                    "source_locator": "core/src/workflow/mod.rs",
                    "source_type": "repository",
                    "confidence": "high",
                }],
                "uncertainties": [],
            })),
            // Verify: one candidate -> one verifier shard.
            MockAgentReply::json(json!({
                "verdicts": [{
                    "claim_id": "claim-0",
                    "supported": true,
                    "reason": "the patch adds the workflow module",
                    "evidence": "core/src/workflow/mod.rs is in the patch",
                    "source_title": "yeax-workflow.patch",
                    "source_locator": "core/src/workflow/mod.rs",
                }],
            })),
            // Report: prose with the packet's citation marker.
            MockAgentReply::text(
                "<report-body>\nThe workflow runtime is migrated [S1].\n</report-body>",
            ),
        ],
    )
    .await
    .expect("engine run");

    assert!(
        report.completed,
        "deep-research must complete: {:?}",
        report.error
    );
    assert_eq!(report.output["status"], json!("verified"));
    assert_eq!(
        report.prompts.len(),
        4,
        "plan, research, verify, and report each drive one mock turn"
    );
    assert!(report.prompts[0].contains("independent questions"));
    assert!(report.prompts[3].contains("verified-findings-json"));
}

#[tokio::test]
#[ignore = "requires a live session: pause/resume is driven through WorkflowManager"]
async fn e2e_03_pause_and_resume() {}

#[tokio::test]
#[ignore = "requires a live session: stop/cancel drains real children"]
async fn e2e_04_stop_and_cancel() {}

#[tokio::test]
#[ignore = "requires app-server/TUI: user-level save round-trip"]
async fn e2e_05_save_user_workflow() {}

#[tokio::test]
#[ignore = "requires model credentials: budget exhaustion needs real agent calls"]
async fn e2e_06_agent_budget_exhausted() {}

#[tokio::test]
#[ignore = "requires model credentials: concurrency caps observed on real children"]
async fn e2e_07_concurrency_limit() {}

#[tokio::test]
#[ignore = "covered at engine level by the 70 workflow-crate tests; no cross-process driver"]
async fn e2e_08_journal_replay() {}

#[tokio::test]
#[ignore = "requires a process restart driver over a persisted run root"]
async fn e2e_09_process_restart() {}

#[tokio::test]
#[ignore = "requires Windows MSVC: Windows path and atomic-save behavior"]
async fn e2e_10_windows_paths_and_atomic_save() {}
