//! CLI parser for the roki binary.
//!
//! Top-level `roki` command exposes one subcommand (`run`) and one flag
//! (`--config <path>` on `run`). [`run`] parses argv, dispatches the
//! matched subcommand, and returns an [`ExitCode`] propagated by `main`.
//!
//! The `--config` doc comment intentionally names `roki.toml` so that
//! `clap`'s rendered `--help` lists the configuration file the flag
//! identifies (Req 1.3).
//!
//! Walking-skeleton task 5.2 wires this module; task 5.3 will call
//! [`run`] from `main`. Until then the module is exercised only by the
//! unit tests below, so suppress `dead_code` for the public surface in
//! line with the convention used by `runtime`, `admission`, etc.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::runtime;
use crate::workflow::{parse, sugar};

/// roki — Linear-driven coding-agent daemon.
#[derive(Debug, Parser)]
#[command(name = "roki", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

/// Typed subcommand surface. Today the binary exposes two subcommands
/// (`run` and `cleanup`); future tasks add admin / diagnostic verbs as
/// additional variants.
#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Start the daemon with default dispatch (cleanup-first then rule).
    Run {
        /// Path to the roki.toml configuration file.
        #[arg(long = "config", value_name = "PATH")]
        config: PathBuf,
    },
    /// Cleanup-only dispatch: only [[cleanup]] matches lead to a cycle.
    /// [[rule]] list is ignored. Same single-shot binary lifecycle as `run`.
    Cleanup {
        /// Path to the roki.toml configuration file.
        #[arg(long = "config", value_name = "PATH")]
        config: PathBuf,
    },
    /// Workflow YAML utilities.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkflowCmd {
    /// Load + sugar-expand + validate a WORKFLOW.yaml file. Exit 0 on success,
    /// non-zero with a multi-error report on validation failure.
    Validate {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Render a rule's state machine as ASCII or DOT.
    Graph {
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Selector form: rules[<idx>] / cleanup[<idx>] / on_failure[<idx>].
        /// Omit to render every state machine in the file.
        #[arg(long = "rule", value_name = "SELECTOR")]
        rule: Option<String>,
        /// Output format. `ascii` prints to stdout-friendly text; `dot` prints
        /// Graphviz DOT.
        #[arg(long = "format", value_name = "FORMAT", default_value = "ascii")]
        format: GraphFormat,
        /// Write to a file instead of stdout.
        #[arg(long = "out", value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum GraphFormat {
    Ascii,
    Dot,
}

/// Parse argv from the process and dispatch the matched subcommand.
///
/// `clap::Parser::parse` exits the process with a non-zero status on a
/// parse error (e.g. missing `--config`), so the caller never observes
/// that failure path here. Successful parses are forwarded to the
/// matching runtime entry point.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Run { config } => runtime::run(&config, runtime::DispatchMode::Default).await,
        CliCommand::Cleanup { config } => {
            runtime::run(&config, runtime::DispatchMode::CleanupOnly).await
        }
        CliCommand::Workflow { cmd } => match cmd {
            WorkflowCmd::Validate { file } => workflow_validate(&file),
            WorkflowCmd::Graph {
                file,
                rule,
                format,
                out,
            } => workflow_graph(&file, rule.as_deref(), format, out.as_deref()),
        },
    }
}

fn workflow_validate(file: &std::path::Path) -> ExitCode {
    let raw = match parse::parse_workflow_file(file) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match sugar::expand(raw, sugar::ExpandConfig::default()) {
        Ok(_) => ExitCode::SUCCESS,
        Err(sugar::ExpandError::Validation(errors)) => {
            for err in &errors {
                eprintln!("{}: {err}", file.display());
            }
            ExitCode::from(2)
        }
        Err(other) => {
            eprintln!("{}: expansion failed: {other}", file.display());
            ExitCode::from(2)
        }
    }
}

fn workflow_graph(
    file: &std::path::Path,
    rule_selector: Option<&str>,
    format: GraphFormat,
    out: Option<&std::path::Path>,
) -> ExitCode {
    let raw = match parse::parse_workflow_file(file) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let workflow = match sugar::expand(raw, sugar::ExpandConfig::default()) {
        Ok(f) => f,
        Err(sugar::ExpandError::Validation(errors)) => {
            for err in &errors {
                eprintln!("{}: {err}", file.display());
            }
            return ExitCode::from(2);
        }
        Err(other) => {
            eprintln!("{}: expansion failed: {other}", file.display());
            return ExitCode::from(2);
        }
    };

    let rendered = match render_graph(&workflow, rule_selector, format) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &rendered) {
                eprintln!("write {}: {e}", path.display());
                return ExitCode::from(1);
            }
        }
        None => print!("{rendered}"),
    }
    ExitCode::SUCCESS
}

fn render_graph(
    workflow: &crate::workflow::canonical::WorkflowFile,
    selector: Option<&str>,
    format: GraphFormat,
) -> Result<String, String> {
    let mut out = String::new();
    let sections: [(&str, &Vec<crate::workflow::canonical::RuleEntry>); 3] = [
        ("rules", &workflow.rules),
        ("cleanup", &workflow.cleanup),
        ("on_failure", &workflow.on_failure),
    ];
    let target = parse_selector(selector)?;

    for (section_name, list) in sections {
        for (idx, rule) in list.iter().enumerate() {
            if let Some((sel_section, sel_idx)) = &target {
                if *sel_section != section_name || *sel_idx != idx {
                    continue;
                }
            }
            match format {
                GraphFormat::Ascii => render_ascii(&mut out, section_name, idx, rule),
                GraphFormat::Dot => render_dot(&mut out, section_name, idx, rule),
            }
        }
    }
    Ok(out)
}

fn parse_selector(s: Option<&str>) -> Result<Option<(&'static str, usize)>, String> {
    let Some(s) = s else { return Ok(None) };
    for section in ["rules", "cleanup", "on_failure"] {
        let prefix = format!("{section}[");
        if let Some(rest) = s.strip_prefix(&prefix) {
            let idx_str = rest
                .strip_suffix(']')
                .ok_or_else(|| format!("invalid selector: {s}"))?;
            let idx: usize = idx_str
                .parse()
                .map_err(|_| format!("invalid selector: {s}"))?;
            return Ok(Some((section, idx)));
        }
    }
    Err(format!(
        "invalid selector '{s}': expected rules[<n>], cleanup[<n>], or on_failure[<n>]"
    ))
}

fn render_ascii(
    out: &mut String,
    section: &str,
    idx: usize,
    rule: &crate::workflow::canonical::RuleEntry,
) {
    use std::fmt::Write;
    let sm = &rule.state_machine;
    let _ = writeln!(out, "# {section}[{idx}]");
    let _ = writeln!(out, "start: {}", sm.start);
    for (id, state) in &sm.states {
        let _ = writeln!(out, "{id} --on_done--> {}", target_name(&state.on_done));
        let _ = writeln!(out, "{id} --on_fail--> {}", target_name(&state.on_fail));
        for (name, target) in &state.directives {
            let _ = writeln!(out, "{id} --[{name}]--> {}", target_name(target));
        }
    }
    for (id, terminal) in &sm.terminals {
        let _ = writeln!(out, "[terminal] {id} outcome={}", terminal.outcome);
    }
    let _ = writeln!(out);
}

fn render_dot(
    out: &mut String,
    section: &str,
    idx: usize,
    rule: &crate::workflow::canonical::RuleEntry,
) {
    use std::fmt::Write;
    let sm = &rule.state_machine;
    let name = format!("{section}_{idx}");
    let _ = writeln!(out, "digraph {name} {{");
    let _ = writeln!(out, "  rankdir=LR;");
    let _ = writeln!(out, "  start [shape=point];");
    let _ = writeln!(out, "  start -> {};", quote_dot(&sm.start));
    for (id, terminal) in &sm.terminals {
        let _ = writeln!(
            out,
            "  {} [shape=doublecircle, label=\"{}\\n[{}]\"];",
            quote_dot(id),
            id,
            terminal.outcome
        );
    }
    for (id, state) in &sm.states {
        let _ = writeln!(out, "  {} [shape=box];", quote_dot(id));
        let _ = writeln!(
            out,
            "  {} -> {} [label=\"on_done\"];",
            quote_dot(id),
            quote_dot(target_id(&state.on_done))
        );
        let _ = writeln!(
            out,
            "  {} -> {} [label=\"on_fail\", style=dashed];",
            quote_dot(id),
            quote_dot(target_id(&state.on_fail))
        );
        for (dname, target) in &state.directives {
            let _ = writeln!(
                out,
                "  {} -> {} [label=\"{}\"];",
                quote_dot(id),
                quote_dot(target_id(target)),
                dname
            );
        }
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
}

fn target_name(t: &crate::workflow::canonical::EdgeTarget) -> String {
    match t {
        crate::workflow::canonical::EdgeTarget::State(id) => id.clone(),
        crate::workflow::canonical::EdgeTarget::Terminal(id) => format!("[{id}]"),
    }
}

fn target_id(t: &crate::workflow::canonical::EdgeTarget) -> &str {
    match t {
        crate::workflow::canonical::EdgeTarget::State(id)
        | crate::workflow::canonical::EdgeTarget::Terminal(id) => id,
    }
}

fn quote_dot(id: &str) -> String {
    format!("\"{id}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn run_subcommand_requires_config_flag() {
        let res = Cli::try_parse_from(["roki", "run"]);
        assert!(res.is_err(), "missing --config should error");
    }

    #[test]
    fn run_with_config_flag_parses() {
        let cli = Cli::try_parse_from(["roki", "run", "--config", "/tmp/roki.toml"])
            .expect("should parse");
        match cli.command {
            CliCommand::Run { config } => {
                assert_eq!(config, PathBuf::from("/tmp/roki.toml"));
            }
            _ => panic!("expected Run variant"),
        }
    }

    #[test]
    fn root_help_lists_run_subcommand() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(
            help.contains("run"),
            "root help should list `run` subcommand: {help}"
        );
    }

    #[test]
    fn run_help_names_config_and_roki_toml() {
        let cli = Cli::command();
        let run_cmd = cli.find_subcommand("run").expect("run subcommand exists");
        let help = run_cmd.clone().render_help().to_string();
        assert!(
            help.contains("--config"),
            "run help missing --config: {help}"
        );
        assert!(
            help.contains("roki.toml"),
            "run help should mention roki.toml: {help}"
        );
    }

    #[test]
    fn cleanup_subcommand_with_config_flag_parses() {
        let cli = Cli::try_parse_from(["roki", "cleanup", "--config", "/tmp/roki.toml"])
            .expect("should parse");
        match cli.command {
            CliCommand::Cleanup { config } => {
                assert_eq!(config, PathBuf::from("/tmp/roki.toml"));
            }
            _ => panic!("expected Cleanup variant"),
        }
    }

    #[test]
    fn root_help_lists_cleanup_subcommand() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(
            help.contains("cleanup"),
            "root help should list cleanup: {help}"
        );
    }
}
