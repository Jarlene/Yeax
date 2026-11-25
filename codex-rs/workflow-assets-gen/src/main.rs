//! CLI entry point for generating and checking embedded workflow assets.

use std::path::PathBuf;
use std::process::ExitCode;

use codex_workflow_assets_gen::AssetError;
use codex_workflow_assets_gen::check;
use codex_workflow_assets_gen::default_assets_dir;
use codex_workflow_assets_gen::default_generated_path;
use codex_workflow_assets_gen::generate;

const USAGE: &str =
    "usage: codex-workflow-assets-gen <--write|--check> [--assets <dir>] [--output <file>]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut mode: Option<bool> = None;
    let mut assets = default_assets_dir();
    let mut output = default_generated_path();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--write" => set_mode(&mut mode, true)?,
            "--check" => set_mode(&mut mode, false)?,
            "--assets" => {
                assets = PathBuf::from(args.next().ok_or("--assets requires a directory")?);
            }
            "--output" => {
                output = PathBuf::from(args.next().ok_or("--output requires a file")?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }

    match mode {
        Some(true) => {
            let source = generate(&assets).map_err(display)?;
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            std::fs::write(&output, source)
                .map_err(|error| format!("{}: {error}", output.display()))?;
            println!("wrote {}", output.display());
            Ok(())
        }
        Some(false) => {
            check(&assets, &output).map_err(display)?;
            println!("assets ok: {}", output.display());
            Ok(())
        }
        None => Err(USAGE.to_string()),
    }
}

fn set_mode(mode: &mut Option<bool>, write: bool) -> Result<(), String> {
    match mode {
        Some(existing) if *existing != write => {
            Err("choose exactly one of --write or --check".to_string())
        }
        _ => {
            *mode = Some(write);
            Ok(())
        }
    }
}

fn display(error: AssetError) -> String {
    error.to_string()
}
