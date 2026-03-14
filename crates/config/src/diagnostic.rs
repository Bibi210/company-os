//! Unified diagnostic format for all Company OS components.
//!
//! Every error, warning, or info message visible to an LLM agent or CI
//! output MUST use this format so that agents can reliably parse failures
//! and decide what to do next.
//!
//! Format:
//! ```text
//! [companyos:orchestrator] ERROR: Review round not found
//! | context: submit_review_vote(round_id="abc-123")
//! | reason:  No open round with this ID exists in the database
//! | fix:     Call initiate_review_round first, or check the round_id
//! ```

use std::fmt;

/// Severity level for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => write!(f, "ERROR"),
            Self::Warning => write!(f, "WARN"),
            Self::Info => write!(f, "OK"),
        }
    }
}

/// A structured diagnostic message that all Company OS components produce.
///
/// Designed to be unambiguous for LLM agents:
/// - `component`: identifies the source (orchestrator, yaml-validator, lessons-kb, pre-commit, defense-in-depth)
/// - `severity`: ERROR, WARN, or OK
/// - `message`: one-line summary of what happened
/// - `context`: what operation was being attempted (tool name, file path, etc.)
/// - `reason`: why it failed — the root cause
/// - `fix`: actionable step the agent should take to resolve it
pub struct Diagnostic {
    pub component: &'static str,
    pub severity: Severity,
    pub message: String,
    pub context: Option<String>,
    pub reason: Option<String>,
    pub fix: Option<String>,
}

impl Diagnostic {
    pub fn error(component: &'static str, message: impl Into<String>) -> Self {
        Self {
            component,
            severity: Severity::Error,
            message: message.into(),
            context: None,
            reason: None,
            fix: None,
        }
    }

    pub fn warning(component: &'static str, message: impl Into<String>) -> Self {
        Self {
            component,
            severity: Severity::Warning,
            message: message.into(),
            context: None,
            reason: None,
            fix: None,
        }
    }

    pub fn info(component: &'static str, message: impl Into<String>) -> Self {
        Self {
            component,
            severity: Severity::Info,
            message: message.into(),
            context: None,
            reason: None,
            fix: None,
        }
    }

    pub fn with_context(mut self, ctx: impl Into<String>) -> Self {
        self.context = Some(ctx.into());
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[companyos:{}] {}: {}",
            self.component, self.severity, self.message
        )?;
        if let Some(ref ctx) = self.context {
            write!(f, "\n| context: {ctx}")?;
        }
        if let Some(ref reason) = self.reason {
            write!(f, "\n| reason:  {reason}")?;
        }
        if let Some(ref fix) = self.fix {
            write!(f, "\n| fix:     {fix}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_format() {
        let d = Diagnostic::error("orchestrator", "Review round not found")
            .with_context("submit_review_vote(round_id=\"abc-123\")")
            .with_reason("No open round with this ID exists in the database")
            .with_fix("Call initiate_review_round first, or check the round_id");

        let output = d.to_string();
        assert!(output.starts_with("[companyos:orchestrator] ERROR: Review round not found"));
        assert!(output.contains("| context:"));
        assert!(output.contains("| reason:"));
        assert!(output.contains("| fix:"));
    }

    #[test]
    fn test_minimal_error() {
        let d = Diagnostic::error("yaml-validator", "Schema validation failed");
        let output = d.to_string();
        assert_eq!(
            output,
            "[companyos:yaml-validator] ERROR: Schema validation failed"
        );
    }

    #[test]
    fn test_warning_format() {
        let d = Diagnostic::warning("pre-commit", "Skipped file");
        let output = d.to_string();
        assert!(output.starts_with("[companyos:pre-commit] WARN: Skipped file"));
    }

    #[test]
    fn test_info_format() {
        let d = Diagnostic::info("yaml-validator", "Valid");
        let output = d.to_string();
        assert!(output.starts_with("[companyos:yaml-validator] OK: Valid"));
    }

    #[test]
    fn test_builder_chain_all_fields() {
        let d = Diagnostic::error("defense-in-depth", "Write blocked")
            .with_context("edit(file=foo.yml)")
            .with_reason("No permit")
            .with_fix("Request a write permit");
        let output = d.to_string();
        assert!(output.contains("| context: edit(file=foo.yml)"));
        assert!(output.contains("| reason:  No permit"));
        assert!(output.contains("| fix:     Request a write permit"));
    }
}
