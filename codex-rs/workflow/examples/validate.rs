//! Example CLI that validates a workflow script read from stdin.

use std::io::Read as _;

fn main() {
    let mut script = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut script) {
        println!("READ FAIL: {error}");
        std::process::exit(3);
    }

    match codex_workflow::validate_script(&script, None) {
        Ok(report) => {
            println!("META OK: name={} phases={}", report.name, report.phases);
            println!("RUN OK: {}", report.outcome_summary);
        }
        Err(codex_workflow::ValidationError::Meta(e)) => {
            println!("META FAIL: {e}");
            std::process::exit(1);
        }
        Err(codex_workflow::ValidationError::Run(e)) => {
            println!("RUN FAIL: {e}");
            std::process::exit(2);
        }
    }
}
