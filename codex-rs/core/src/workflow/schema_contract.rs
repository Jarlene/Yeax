//! Output-schema contract: safety checks, prompt suffix, candidate extraction,
//! and one-shot revalidation support.
//!
//! External `$ref` is rejected because resolving it would need network access or
//! arbitrary filesystem reads. Internal references (`#/...`) are allowed.

use std::fmt;

use serde_json::Value;

/// Attempt 1 plus exactly one repair turn.
pub(crate) const MAX_SCHEMA_ATTEMPTS: u8 = 2;
/// Maximum serialized schema size accepted from a script.
pub(crate) const MAX_SCHEMA_BYTES: usize = 256 * 1024;
/// Maximum agent output scanned for a JSON candidate.
pub(crate) const MAX_OUTPUT_BYTES: usize = 1_048_576;

pub(crate) const OUTPUT_CONTRACT_OPEN: &str = "<output-contract>";
pub(crate) const OUTPUT_CONTRACT_CLOSE: &str = "</output-contract>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaError {
    InvalidSchema(String),
    TooLarge { actual: usize, max: usize },
    MissingJson,
    InvalidJson(String),
    ValidationFailed(Vec<String>),
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(reason) => write!(f, "invalid output schema: {reason}"),
            Self::TooLarge { actual, max } => {
                write!(f, "output schema is {actual} bytes, maximum is {max}")
            }
            Self::MissingJson => write!(f, "agent output contains no JSON value"),
            Self::InvalidJson(reason) => write!(f, "agent output is not valid JSON: {reason}"),
            Self::ValidationFailed(errors) => {
                write!(
                    f,
                    "agent output does not match the schema: {}",
                    errors.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for SchemaError {}

#[derive(Debug, Clone)]
pub(crate) struct SchemaContract {
    schema: Value,
    canonical: String,
}

impl SchemaContract {
    pub(crate) fn parse(schema: Value) -> Result<Self, SchemaError> {
        if !matches!(schema, Value::Object(_) | Value::Bool(_)) {
            return Err(SchemaError::InvalidSchema(
                "root must be a JSON object or boolean".to_string(),
            ));
        }
        reject_external_refs(&schema, "")?;
        let canonical = serde_json::to_string(&schema)
            .map_err(|error| SchemaError::InvalidSchema(error.to_string()))?;
        if canonical.len() > MAX_SCHEMA_BYTES {
            return Err(SchemaError::TooLarge {
                actual: canonical.len(),
                max: MAX_SCHEMA_BYTES,
            });
        }
        jsonschema::validator_for(&schema)
            .map_err(|error| SchemaError::InvalidSchema(error.to_string()))?;
        Ok(Self { schema, canonical })
    }

    /// Human/model readable contract appended to the child prompt.
    pub(crate) fn prompt_suffix(&self) -> String {
        format!(
            "{OUTPUT_CONTRACT_OPEN}\nRespond with a single JSON value that validates against \
             this JSON Schema. Do not wrap it in prose.\n{}\n{OUTPUT_CONTRACT_CLOSE}",
            self.canonical
        )
    }

    /// Repair prompt for the single retry attempt.
    pub(crate) fn retry_prompt(&self, errors: &[String]) -> String {
        let mut message = String::from(
            "Your previous reply did not satisfy the required JSON schema. \
             Reply again with only a corrected JSON value.",
        );
        if !errors.is_empty() {
            message.push_str("\nValidation errors:\n");
            for error in errors {
                message.push_str("- ");
                message.push_str(error);
                message.push('\n');
            }
        }
        message.push_str(&self.prompt_suffix());
        message
    }

    /// Parse and validate a child's final text.
    pub(crate) fn validate_text(&self, text: &str) -> Result<Value, SchemaError> {
        if text.len() > MAX_OUTPUT_BYTES {
            return Err(SchemaError::InvalidJson(format!(
                "agent output is {} bytes, maximum is {MAX_OUTPUT_BYTES}",
                text.len()
            )));
        }
        let candidate = extract_json_candidate(text).ok_or(SchemaError::MissingJson)?;
        let instance: Value = serde_json::from_str(&candidate)
            .map_err(|error| SchemaError::InvalidJson(error.to_string()))?;
        let validator = jsonschema::validator_for(&self.schema)
            .map_err(|error| SchemaError::InvalidSchema(error.to_string()))?;
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect();
        if errors.is_empty() {
            Ok(instance)
        } else {
            Err(SchemaError::ValidationFailed(errors))
        }
    }
}

/// Extract the first fenced JSON block, otherwise the first balanced JSON
/// object or array in the text.
pub(crate) fn extract_json_candidate(text: &str) -> Option<String> {
    if let Some(fenced) = fenced_json(text) {
        return Some(fenced);
    }
    balanced_json(text)
}

fn fenced_json(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let index = after.find('\n')?;
        let language = &after[..index];
        let body_start = &after[index + 1..];
        let is_json = language.trim().eq_ignore_ascii_case("json")
            || language.trim().eq_ignore_ascii_case("jsonc")
            || language.trim().is_empty();
        let end = body_start.find("```")?;
        if is_json {
            let candidate = body_start[..end].trim();
            if serde_json::from_str::<Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
        rest = &body_start[end + 3..];
    }
    None
}

fn balanced_json(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for (start, byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'{' | b'[') {
            continue;
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for (offset, candidate) in bytes.iter().enumerate().skip(start) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *candidate == b'\\' {
                    escaped = true;
                } else if *candidate == b'"' {
                    in_string = false;
                }
                continue;
            }
            match candidate {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = text.get(start..=offset)?;
                        if serde_json::from_str::<Value>(candidate).is_ok() {
                            return Some(candidate.to_string());
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn reject_external_refs(value: &Value, path: &str) -> Result<(), SchemaError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}/{key}");
                if key == "$ref" {
                    let reference = child.as_str().ok_or_else(|| {
                        SchemaError::InvalidSchema(format!("{child_path} must be a string"))
                    })?;
                    if !reference.starts_with('#') {
                        return Err(SchemaError::InvalidSchema(format!(
                            "{child_path} uses external reference `{reference}`; \
                             only internal `#/...` references are allowed"
                        )));
                    }
                }
                reject_external_refs(child, &child_path)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                reject_external_refs(child, &format!("{path}/{index}"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "schema_contract_tests.rs"]
mod tests;
