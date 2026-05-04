//! `WORKFLOW.md` parser: front matter + Liquid + named template blocks.
//!
//! Front matter MUST be the first thing in the file: either `---\n...\n---`
//! (YAML, default) or `+++\n...\n+++` (TOML). The body is then rendered as a
//! Liquid template against the per-load variable map (currently empty; the
//! per-orchestrator and per-phase variables substitute at render time, not
//! load time). After render the body is scanned for `## prompt_template_<name>`
//! H2 headings; everything between one such heading and the next H2 (or EOF)
//! is the block body. A leading fenced code block (```...```) is unwrapped so
//! operators may keep the rendered prompt syntax-highlighted in editors.
//!
//! Spec refs: requirements.md Req 2.15, Req 6.1.

use std::collections::BTreeMap;

use thiserror::Error;

/// Names of the four template blocks that MUST be present at startup.
pub const REQUIRED_BLOCKS: &[&str] = &[
    "prompt_template_orchestrator",
    "prompt_template_implement_direct",
    "prompt_template_validate_direct",
    "prompt_template_open_pr",
];

/// Optional per-phase override block names recognized by the loader. Other
/// `prompt_template_*` headings are still captured so downstream
/// schema/validation can decide what to do with them.
pub const OPTIONAL_PHASE_BLOCKS: &[&str] = &[
    "prompt_template_classify",
    "prompt_template_implement",
    "prompt_template_review",
    "prompt_template_validate",
    "prompt_template_open_pr",
    "prompt_template_ci_fix",
    "prompt_template_finalize_review",
];

/// Output of [`parse_str`]: front-matter as a JSON value (uniform shape across
/// YAML / TOML inputs) plus the named template blocks keyed by their heading
/// name (`prompt_template_<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWorkflow {
    pub front_matter: serde_json::Value,
    pub blocks: BTreeMap<String, String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// File does not start with `---` or `+++` delimiter.
    #[error("missing front-matter delimiter (expected `---` for YAML or `+++` for TOML at start of file)")]
    MissingDelimiter,

    /// Front-matter parse error (YAML or TOML).
    #[error("front-matter parse error: {0}")]
    FrontMatter(String),

    /// Liquid template parse / render error.
    #[error("Liquid render error: {0}")]
    Liquid(String),

    /// One of [`REQUIRED_BLOCKS`] is missing.
    #[error("required named template block `{0}` is missing from WORKFLOW.md")]
    MissingRequiredBlock(String),
}

/// Parse a `WORKFLOW.md` source string. The render-time variable map is
/// intentionally empty here: per-render variables (issue id, mode, etc.)
/// substitute at orchestrator / phase render, not at load. A bare
/// `{{ variable }}` in the source survives the load-time render unchanged so
/// long as the variable is unknown — Liquid leaves undefined variables empty
/// rather than erroring, matching operator expectations from upstream
/// `WORKFLOW.md` examples.
pub fn parse_str(text: &str) -> Result<ParsedWorkflow, ParseError> {
    let (front_matter_raw, kind, body) = split_front_matter(text)?;
    let front_matter = parse_front_matter(front_matter_raw, kind)?;
    let rendered = render_body(body)?;
    let blocks = extract_blocks(&rendered);

    for required in REQUIRED_BLOCKS {
        if !blocks.contains_key(*required) {
            return Err(ParseError::MissingRequiredBlock((*required).to_owned()));
        }
    }

    Ok(ParsedWorkflow {
        front_matter,
        blocks,
    })
}

#[derive(Debug, Clone, Copy)]
enum FrontMatterKind {
    Yaml,
    Toml,
}

/// Split off the leading front-matter block. Returns `(raw_front_matter,
/// kind, body)`. The body starts immediately after the closing delimiter
/// line.
fn split_front_matter(text: &str) -> Result<(&str, FrontMatterKind, &str), ParseError> {
    let (open_marker, kind) = if text.starts_with("---") {
        ("---", FrontMatterKind::Yaml)
    } else if text.starts_with("+++") {
        ("+++", FrontMatterKind::Toml)
    } else {
        return Err(ParseError::MissingDelimiter);
    };

    // Confirm the opener is a complete line (followed by `\n`), otherwise
    // `---` could match an opening Markdown rule by accident.
    let after_open = text
        .strip_prefix(open_marker)
        .and_then(|rest| rest.strip_prefix('\n'))
        .ok_or(ParseError::MissingDelimiter)?;

    // Support both forms:
    //   ---            ---
    //   key: value     ---
    //   ---            (empty front matter directly closing)
    //   <body>         <body>
    // The closing line must be the marker followed by `\n` or EOF.
    let prefix_close = format!("{open_marker}\n");
    let close_idx = if after_open.starts_with(&prefix_close) {
        // Empty front matter: closing marker at offset 0.
        Some(0usize)
    } else if after_open == open_marker {
        Some(0usize)
    } else {
        let with_newline = format!("\n{open_marker}\n");
        let trailing = format!("\n{open_marker}");
        if let Some(idx) = after_open.find(&with_newline) {
            Some(idx + 1)
        } else if after_open.ends_with(&trailing) {
            Some(after_open.len() - open_marker.len())
        } else {
            None
        }
    }
    .ok_or(ParseError::MissingDelimiter)?;

    let raw = &after_open[..close_idx.saturating_sub(0)];
    // Strip trailing newline embedded in raw range.
    let raw = raw.strip_suffix('\n').unwrap_or(raw);

    // Skip the close marker plus its (optional) trailing newline.
    let after_close = &after_open[close_idx..];
    let after_close = after_close
        .strip_prefix(open_marker)
        .ok_or(ParseError::MissingDelimiter)?;
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);
    Ok((raw, kind, body))
}

fn parse_front_matter(raw: &str, kind: FrontMatterKind) -> Result<serde_json::Value, ParseError> {
    match kind {
        FrontMatterKind::Yaml => {
            // Empty YAML is legal: emit a JSON null so downstream defaulting
            // can apply uniformly.
            if raw.trim().is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let yaml: serde_yaml::Value = serde_yaml::from_str(raw)
                .map_err(|e| ParseError::FrontMatter(e.to_string()))?;
            yaml_to_json(yaml).map_err(ParseError::FrontMatter)
        }
        FrontMatterKind::Toml => {
            if raw.trim().is_empty() {
                return Ok(serde_json::Value::Object(serde_json::Map::new()));
            }
            let value: toml::Value = toml::from_str(raw)
                .map_err(|e| ParseError::FrontMatter(e.to_string()))?;
            toml_to_json(value).map_err(ParseError::FrontMatter)
        }
    }
}

fn yaml_to_json(value: serde_yaml::Value) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

fn toml_to_json(value: toml::Value) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

fn render_body(body: &str) -> Result<String, ParseError> {
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|e| ParseError::Liquid(e.to_string()))?;
    let template = parser
        .parse(body)
        .map_err(|e| ParseError::Liquid(e.to_string()))?;
    let globals = liquid::Object::new();
    template
        .render(&globals)
        .map_err(|e| ParseError::Liquid(e.to_string()))
}

/// Walk the rendered body line-by-line and split it into named blocks at
/// every `## prompt_template_<name>` heading.
fn extract_blocks(rendered: &str) -> BTreeMap<String, String> {
    let mut blocks: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;

    for line in rendered.lines() {
        if let Some(name) = parse_h2_template_heading(line) {
            if let Some((existing_name, existing_lines)) = current.take() {
                blocks.insert(existing_name, finalize_block(&existing_lines));
            }
            current = Some((name, Vec::new()));
        } else if line.starts_with("## ") {
            // Any other H2 closes the current block without starting a new one.
            if let Some((existing_name, existing_lines)) = current.take() {
                blocks.insert(existing_name, finalize_block(&existing_lines));
            }
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }

    if let Some((name, lines)) = current.take() {
        blocks.insert(name, finalize_block(&lines));
    }

    blocks
}

/// If `line` is exactly `## prompt_template_<name>`, return `<name>` keyed
/// with the `prompt_template_` prefix; otherwise `None`. Trailing whitespace
/// is tolerated.
fn parse_h2_template_heading(line: &str) -> Option<String> {
    let rest = line.strip_prefix("## ")?;
    let rest = rest.trim_end();
    if rest.starts_with("prompt_template_") && !rest.contains(char::is_whitespace) {
        Some(rest.to_owned())
    } else {
        None
    }
}

/// Trim leading/trailing blank lines from a captured block and unwrap a
/// single fenced code block (```...```), if present, so operators may keep
/// the rendered prompt fenced for editor highlighting.
fn finalize_block(lines: &[&str]) -> String {
    let mut start = 0usize;
    let mut end = lines.len();
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let trimmed = &lines[start..end];

    if trimmed.len() >= 2
        && trimmed.first().is_some_and(|first| first.trim_start().starts_with("```"))
        && trimmed.last().is_some_and(|last| last.trim() == "```")
    {
        // Unwrap the fence: drop first + last line.
        return trimmed[1..trimmed.len() - 1].join("\n");
    }

    trimmed.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_workflow(extra: &str) -> String {
        format!(
            "---\nfoo: bar\n---\n\
            ## prompt_template_orchestrator\n\
            orchestrator body\n\
            \n\
            ## prompt_template_implement_direct\n\
            implement body\n\
            \n\
            ## prompt_template_validate_direct\n\
            validate body\n\
            \n\
            ## prompt_template_open_pr\n\
            open_pr body\n\
            {extra}",
        )
    }

    #[test]
    fn parses_yaml_front_matter_and_all_required_blocks() {
        let extra = "\n## prompt_template_review\nreview override\n\
                     \n## prompt_template_ci_fix\nci fix override\n";
        let parsed = parse_str(&full_workflow(extra)).expect("parses");

        assert_eq!(
            parsed.front_matter,
            serde_json::json!({"foo": "bar"}),
            "yaml front matter must round-trip as json object",
        );

        for required in REQUIRED_BLOCKS {
            assert!(
                parsed.blocks.contains_key(*required),
                "required block `{required}` missing",
            );
            assert!(
                !parsed.blocks[*required].trim().is_empty(),
                "required block `{required}` must be non-empty",
            );
        }
        assert!(parsed.blocks["prompt_template_orchestrator"].contains("orchestrator body"));
        assert!(parsed.blocks["prompt_template_implement_direct"].contains("implement body"));
        assert!(parsed.blocks["prompt_template_validate_direct"].contains("validate body"));
        assert!(parsed.blocks["prompt_template_open_pr"].contains("open_pr body"));

        assert!(parsed.blocks.contains_key("prompt_template_review"));
        assert_eq!(parsed.blocks["prompt_template_review"].trim(), "review override");
        assert!(parsed.blocks.contains_key("prompt_template_ci_fix"));
        assert_eq!(parsed.blocks["prompt_template_ci_fix"].trim(), "ci fix override");
    }

    #[test]
    fn refuses_when_required_block_missing() {
        let body = "---\nfoo: bar\n---\n\
                    ## prompt_template_orchestrator\norch\n\
                    \n## prompt_template_implement_direct\nimpl\n\
                    \n## prompt_template_validate_direct\nval\n";
        let err = parse_str(body).unwrap_err();
        assert_eq!(
            err,
            ParseError::MissingRequiredBlock("prompt_template_open_pr".to_owned()),
        );
        assert!(
            err.to_string().contains("prompt_template_open_pr"),
            "error message must name the missing block: {err}",
        );
    }

    #[test]
    fn parses_toml_front_matter() {
        let body = "+++\nname = \"workflow\"\n+++\n\
                    ## prompt_template_orchestrator\norch\n\
                    \n## prompt_template_implement_direct\nimpl\n\
                    \n## prompt_template_validate_direct\nval\n\
                    \n## prompt_template_open_pr\nopen\n";
        let parsed = parse_str(body).expect("toml front matter parses");
        assert_eq!(
            parsed.front_matter,
            serde_json::json!({"name": "workflow"}),
        );
    }

    #[test]
    fn refuses_when_no_front_matter_delimiter() {
        let err = parse_str("hello world").unwrap_err();
        assert_eq!(err, ParseError::MissingDelimiter);
    }

    #[test]
    fn fenced_block_body_is_unwrapped() {
        let body = "---\n---\n\
                    ## prompt_template_orchestrator\n\
                    ```\nfenced orch body\n```\n\
                    \n## prompt_template_implement_direct\nimpl\n\
                    \n## prompt_template_validate_direct\nval\n\
                    \n## prompt_template_open_pr\nopen\n";
        let parsed = parse_str(body).unwrap();
        assert_eq!(
            parsed.blocks["prompt_template_orchestrator"],
            "fenced orch body",
        );
    }

    #[test]
    fn unrelated_h2_does_not_attach_to_previous_block() {
        let body = "---\n---\n\
                    ## prompt_template_orchestrator\norch line\n\
                    \n## Notes\nshould not attach\n\
                    \n## prompt_template_implement_direct\nimpl\n\
                    \n## prompt_template_validate_direct\nval\n\
                    \n## prompt_template_open_pr\nopen\n";
        let parsed = parse_str(body).unwrap();
        assert_eq!(parsed.blocks["prompt_template_orchestrator"].trim(), "orch line");
        assert!(!parsed.blocks["prompt_template_orchestrator"].contains("should not attach"));
    }

    #[test]
    fn liquid_render_substitutes_known_filters() {
        // Operators may use Liquid stdlib filters in the body. Unknown
        // variables render to empty per Liquid semantics, which we rely on
        // since per-render variables substitute later.
        let body = "---\n---\n\
                    ## prompt_template_orchestrator\nhello {{ \"world\" | upcase }}\n\
                    \n## prompt_template_implement_direct\nimpl\n\
                    \n## prompt_template_validate_direct\nval\n\
                    \n## prompt_template_open_pr\nopen\n";
        let parsed = parse_str(body).unwrap();
        assert!(
            parsed.blocks["prompt_template_orchestrator"].contains("WORLD"),
            "Liquid filter should have rendered: {:?}",
            parsed.blocks["prompt_template_orchestrator"],
        );
    }
}
