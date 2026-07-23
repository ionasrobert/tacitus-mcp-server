use std::fmt;

/// Structured, actionable error (mirrors the TS `MindVaultError`/`StructuredError`).
/// Never a bare "failed": every error says what went wrong AND what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TacitusError {
    pub code: String,
    pub reason: String,
    pub suggestion: String,
}

impl TacitusError {
    pub fn new(code: &str, reason: impl Into<String>, suggestion: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            reason: reason.into(),
            suggestion: suggestion.into(),
        }
    }
}

impl fmt::Display for TacitusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for TacitusError {}
