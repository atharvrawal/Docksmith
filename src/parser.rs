//! parser.rs — Docksmithfile parser.
//!
//! Supported instructions: FROM, COPY, RUN, WORKDIR, ENV, CMD
//! Any unrecognised instruction fails with the line number.

use anyhow::{bail, Context, Result};
use std::path::Path;

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// FROM <image>[:<tag>]
    From { image_ref: String },

    /// COPY <src_glob> <dest>
    Copy { src: String, dest: String },

    /// RUN <shell command>
    Run { command: String },

    /// WORKDIR <path>
    WorkDir { path: String },

    /// ENV key=value
    Env { key: String, value: String },

    /// CMD ["exec", "arg", ...]
    Cmd { args: Vec<String> },
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a Docksmithfile at the given path.
/// Returns an ordered list of instructions.
pub fn parse_file(path: &Path) -> Result<Vec<Instruction>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read Docksmithfile at {:?}", path))?;
    parse_str(&src)
}

/// Parse a Docksmithfile from a string (useful for tests).
pub fn parse_str(src: &str) -> Result<Vec<Instruction>> {
    let mut instructions = Vec::new();

    for (line_no, line) in src.lines().enumerate() {
        let line_no = line_no + 1;     // 1-based for error messages
        let trimmed = line.trim();

        // Skip blank lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let instr = parse_line(trimmed, line_no)?;
        instructions.push(instr);
    }

    Ok(instructions)
}

fn parse_line(line: &str, line_no: usize) -> Result<Instruction> {
    // Split at first whitespace to get the keyword
    let (keyword, rest) = match line.split_once(|c: char| c.is_whitespace()) {
        Some((k, r)) => (k.to_uppercase(), r.trim().to_string()),
        None         => (line.to_uppercase(), String::new()),
    };

    match keyword.as_str() {
        "FROM" => parse_from(&rest, line_no),
        "COPY" => parse_copy(&rest, line_no),
        "RUN"  => parse_run(&rest, line_no),
        "WORKDIR" => parse_workdir(&rest, line_no),
        "ENV"  => parse_env(&rest, line_no),
        "CMD"  => parse_cmd(&rest, line_no),
        other  => bail!(
            "line {}: unknown instruction '{}' \
             (supported: FROM, COPY, RUN, WORKDIR, ENV, CMD)",
            line_no, other
        ),
    }
}

// ---------------------------------------------------------------------------
// Individual instruction parsers
// ---------------------------------------------------------------------------

fn parse_from(rest: &str, line_no: usize) -> Result<Instruction> {
    let image_ref = rest.trim().to_string();
    if image_ref.is_empty() {
        bail!("line {}: FROM requires an image reference (e.g. FROM alpine:3.18)", line_no);
    }
    Ok(Instruction::From { image_ref })
}

fn parse_copy(rest: &str, line_no: usize) -> Result<Instruction> {
    // COPY expects exactly two tokens: <src> <dest>
    // They may be quoted, but we keep it simple: split on whitespace,
    // allow exactly two tokens (dest can contain spaces if quoted — not required by spec)
    let parts: Vec<&str> = rest.splitn(2, |c: char| c.is_whitespace()).collect();
    if parts.len() != 2 || parts[1].trim().is_empty() {
        bail!(
            "line {}: COPY requires exactly two arguments: COPY <src> <dest>",
            line_no
        );
    }
    Ok(Instruction::Copy {
        src:  parts[0].trim().to_string(),
        dest: parts[1].trim().to_string(),
    })
}

fn parse_run(rest: &str, line_no: usize) -> Result<Instruction> {
    if rest.trim().is_empty() {
        bail!("line {}: RUN requires a command", line_no);
    }
    Ok(Instruction::Run { command: rest.to_string() })
}

fn parse_workdir(rest: &str, line_no: usize) -> Result<Instruction> {
    let path = rest.trim().to_string();
    if path.is_empty() {
        bail!("line {}: WORKDIR requires a path", line_no);
    }
    Ok(Instruction::WorkDir { path })
}

fn parse_env(rest: &str, line_no: usize) -> Result<Instruction> {
    // ENV key=value  (only the k=v form is required)
    match rest.split_once('=') {
        Some((k, v)) => {
            let key = k.trim().to_string();
            if key.is_empty() {
                bail!("line {}: ENV key cannot be empty", line_no);
            }
            Ok(Instruction::Env { key, value: v.to_string() })
        }
        None => bail!(
            "line {}: ENV requires key=value syntax (e.g. ENV APP_ENV=production)",
            line_no
        ),
    }
}

fn parse_cmd(rest: &str, line_no: usize) -> Result<Instruction> {
    // CMD must be in JSON array form: ["exec", "arg1", ...]
    let trimmed = rest.trim();
    if !trimmed.starts_with('[') {
        bail!(
            "line {}: CMD must use JSON array form, e.g. CMD [\"sh\",\"-c\",\"echo hi\"]",
            line_no
        );
    }
    let args: Vec<String> = serde_json::from_str(trimmed)
        .with_context(|| format!("line {}: CMD JSON parse error", line_no))?;
    if args.is_empty() {
        bail!("line {}: CMD array must not be empty", line_no);
    }
    Ok(Instruction::Cmd { args })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_dockerfile() {
        let src = r#"
FROM alpine:3.18
WORKDIR /app
ENV APP_ENV=production
COPY . /app
RUN echo hello
CMD ["sh", "-c", "echo done"]
"#;
        let instrs = parse_str(src).unwrap();
        assert_eq!(instrs.len(), 6);
        assert_eq!(instrs[0], Instruction::From { image_ref: "alpine:3.18".to_string() });
        assert_eq!(instrs[1], Instruction::WorkDir { path: "/app".to_string() });
        assert_eq!(instrs[2], Instruction::Env { key: "APP_ENV".to_string(), value: "production".to_string() });
        assert_eq!(instrs[3], Instruction::Copy { src: ".".to_string(), dest: "/app".to_string() });
        assert_eq!(instrs[4], Instruction::Run { command: "echo hello".to_string() });
        assert_eq!(instrs[5], Instruction::Cmd { args: vec!["sh".to_string(), "-c".to_string(), "echo done".to_string()] });
    }

    #[test]
    fn unknown_instruction_fails_with_line_number() {
        let src = "FROM alpine:3.18\nEXPOSE 8080\n";
        let err = parse_str(src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("line 2"), "error must include line number: {}", msg);
        assert!(msg.contains("EXPOSE"), "error must name the bad instruction: {}", msg);
    }

    #[test]
    fn cmd_must_be_json_array() {
        let err = parse_str("FROM a\nCMD sh -c echo\n").unwrap_err();
        assert!(err.to_string().contains("JSON array"));
    }

    #[test]
    fn env_requires_equals() {
        let err = parse_str("FROM a\nENV MYVAR\n").unwrap_err();
        assert!(err.to_string().contains("key=value"));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let src = "\n# this is a comment\nFROM alpine:3.18\n\n";
        let instrs = parse_str(src).unwrap();
        assert_eq!(instrs.len(), 1);
    }

    #[test]
    fn copy_requires_two_args() {
        let err = parse_str("FROM a\nCOPY src\n").unwrap_err();
        assert!(err.to_string().contains("two arguments"));
    }
}
