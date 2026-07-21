//! Safe contracts and planning helpers for RustFS server configuration.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};

/// A server configuration document. Its contents are intentionally omitted from debug output.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ConfigDocument {
    pub content: String,
}

impl std::fmt::Debug for ConfigDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigDocument")
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ConfigHistoryEntry {
    #[serde(rename = "RestoreID")]
    pub restore_id: String,
    #[serde(rename = "CreateTime")]
    pub create_time: String,
    #[serde(rename = "Data", default)]
    pub data: Option<String>,
}

impl std::fmt::Debug for ConfigHistoryEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigHistoryEntry")
            .field("restore_id", &self.restore_id)
            .field("create_time", &self.create_time)
            .field("data", &self.data.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigHelp {
    #[serde(rename = "subSys")]
    pub subsystem: String,
    pub description: String,
    #[serde(rename = "multipleTargets")]
    pub multiple_targets: bool,
    #[serde(rename = "keysHelp")]
    pub keys: Vec<ConfigHelpEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigHelpEntry {
    pub key: String,
    #[serde(rename = "type")]
    pub value_type: String,
    pub description: String,
    pub optional: bool,
    #[serde(rename = "multipleTargets")]
    pub multiple_targets: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigMutationResult {
    pub applied_dynamically: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleSwitches {
    pub notify_enabled: bool,
    pub audit_enabled: bool,
    pub persisted_notify_enabled: bool,
    pub persisted_audit_enabled: bool,
    pub notify_source: String,
    pub audit_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigChange {
    pub scope: String,
    pub key: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigDiff {
    pub changes: Vec<ConfigChange>,
    pub replaces_full_config: bool,
}

/// RustFS Admin API configuration operations.
#[async_trait]
pub trait ConfigApi: Send + Sync {
    async fn get_config(&self, selector: &str) -> Result<ConfigDocument>;
    async fn get_full_config(&self) -> Result<ConfigDocument>;
    async fn set_config(&self, directive: &str) -> Result<ConfigMutationResult>;
    async fn delete_config(&self, directive: &str) -> Result<ConfigMutationResult>;
    async fn config_help(
        &self,
        subsystem: Option<&str>,
        key: Option<&str>,
        env_only: bool,
    ) -> Result<ConfigHelp>;
    async fn config_history(&self, count: usize) -> Result<Vec<ConfigHistoryEntry>>;
    async fn restore_config(&self, restore_id: &str) -> Result<()>;
    async fn import_config(&self, document: &ConfigDocument) -> Result<()>;
    async fn get_module_switches(&self) -> Result<ModuleSwitches>;
    async fn set_module_switches(&self, switches: &ModuleSwitches) -> Result<ModuleSwitches>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedDirective {
    scope: String,
    entries: Vec<(String, Option<String>)>,
}

fn tokenize(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                current.push(character);
            }
        } else {
            match character {
                '\'' | '"' => quote = Some(character),
                value if value.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(character),
            }
        }
    }

    if quote.is_some() {
        return Err(Error::Config(
            "Configuration contains an unterminated quoted value".to_string(),
        ));
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.:".contains(character))
}

fn parse_directives(input: &str, allow_bare_keys: bool) -> Result<Vec<ParsedDirective>> {
    let mut directives = Vec::new();
    for line in input.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens = tokenize(line)?;
        let Some(scope) = tokens.first() else {
            continue;
        };
        if !valid_name(scope) || scope.starts_with(':') || scope.ends_with(':') {
            return Err(Error::Config("Invalid configuration scope".to_string()));
        }

        let mut entries = Vec::new();
        for token in tokens.iter().skip(1) {
            let (key, value) = match token.split_once('=') {
                Some((key, value)) => (key, Some(value.to_string())),
                None if allow_bare_keys => (token.as_str(), None),
                None => {
                    return Err(Error::Config(
                        "Configuration assignments must use key=value syntax".to_string(),
                    ));
                }
            };
            if !valid_name(key) || key.contains(':') {
                return Err(Error::Config("Invalid configuration key".to_string()));
            }
            entries.push((key.to_string(), value));
        }
        directives.push(ParsedDirective {
            scope: scope.to_string(),
            entries,
        });
    }
    if directives.is_empty() {
        return Err(Error::Config(
            "Configuration document must include at least one directive".to_string(),
        ));
    }
    Ok(directives)
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    key.contains("secret")
        || key.contains("password")
        || key == "token"
        || key.ends_with("_token")
        || key == "client_key"
        || key.ends_with("_client_key")
        || key == "private_key"
        || key.ends_with("_private_key")
        || key == "credential"
        || key.ends_with("_credential")
}

fn redacted_value(key: &str, value: &str) -> String {
    if is_sensitive_key(key) && !value.is_empty() {
        "*redacted*".to_string()
    } else {
        value.to_string()
    }
}

fn document_map(input: &str, allow_bare_keys: bool) -> Result<BTreeMap<(String, String), String>> {
    let mut values = BTreeMap::new();
    for directive in parse_directives(input, allow_bare_keys)? {
        for (key, value) in directive.entries {
            if let Some(value) = value {
                values.insert((directive.scope.clone(), key), value);
            }
        }
    }
    Ok(values)
}

/// Validate the syntax accepted by the RustFS config mutation endpoints.
pub fn validate_config_directive(input: &str, allow_bare_keys: bool) -> Result<()> {
    parse_directives(input, allow_bare_keys).map(|_| ())
}

/// Return subsystem/key pairs without exposing configuration values.
pub fn config_document_fields(input: &str) -> Result<Vec<(String, String)>> {
    let mut fields = Vec::new();
    for directive in parse_directives(input, false)? {
        let subsystem = directive
            .scope
            .split_once(':')
            .map_or(directive.scope.as_str(), |(value, _)| value)
            .to_string();
        for (key, _) in directive.entries {
            fields.push((subsystem.clone(), key));
        }
    }
    Ok(fields)
}

/// Reject redacted placeholders before a full replacement can overwrite live secrets.
pub fn validate_config_import(input: &str) -> Result<()> {
    for directive in parse_directives(input, false)? {
        for (key, value) in directive.entries {
            if is_sensitive_key(&key) && value.as_deref() == Some("*redacted*") {
                return Err(Error::Config(
                    "Redacted secret placeholders cannot be imported".to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// Redact secret-bearing fields in a RustFS line-oriented configuration document.
pub fn redact_config_document(input: &str) -> String {
    let Ok(directives) = parse_directives(input, true) else {
        return "<redacted invalid configuration>".to_string();
    };
    directives
        .into_iter()
        .map(|directive| {
            let entries = directive
                .entries
                .into_iter()
                .map(|(key, value)| match value {
                    Some(value) => format!(
                        "{key}=\"{}\"",
                        escape_config_value(&redacted_value(&key, &value))
                    ),
                    None => key,
                })
                .collect::<Vec<_>>();
            if entries.is_empty() {
                directive.scope
            } else {
                format!("{} {}", directive.scope, entries.join(" "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_config_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn change(
    scope: String,
    key: String,
    before: Option<String>,
    after: Option<String>,
) -> ConfigChange {
    let sensitive = is_sensitive_key(&key);
    let before = before.map(|value| redacted_value(&key, &value));
    let after = after.map(|value| redacted_value(&key, &value));
    ConfigChange {
        scope,
        key,
        before,
        after,
        sensitive,
    }
}

/// Build a client-side preflight diff for a set or delete mutation.
pub fn config_mutation_diff(current: &str, directive: &str, delete: bool) -> Result<ConfigDiff> {
    let current_values = document_map(current, false)?;
    let directives = parse_directives(directive, delete)?;
    let mut changes = Vec::new();

    for directive in directives {
        if delete && directive.entries.is_empty() {
            for ((scope, key), before) in current_values
                .iter()
                .filter(|((scope, _), _)| scope == &directive.scope)
            {
                changes.push(change(
                    scope.clone(),
                    key.clone(),
                    Some(before.clone()),
                    None,
                ));
            }
            continue;
        }

        for (key, after) in directive.entries {
            let before = current_values
                .get(&(directive.scope.clone(), key.clone()))
                .cloned();
            if delete {
                if before.is_some() {
                    changes.push(change(directive.scope.clone(), key, before, None));
                }
            } else if before != after {
                changes.push(change(directive.scope.clone(), key, before, after));
            }
        }
    }

    Ok(ConfigDiff {
        changes,
        replaces_full_config: false,
    })
}

/// Build a client-side preflight diff for a full configuration replacement.
pub fn config_import_diff(current: &str, replacement: &str) -> Result<ConfigDiff> {
    let current_values = document_map(current, false)?;
    let replacement_values = document_map(replacement, false)?;
    let keys = current_values
        .keys()
        .chain(replacement_values.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();
    for (scope, key) in keys {
        let before = current_values.get(&(scope.clone(), key.clone())).cloned();
        let after = replacement_values
            .get(&(scope.clone(), key.clone()))
            .cloned();
        if before != after {
            changes.push(change(scope, key, before, after));
        }
    }
    Ok(ConfigDiff {
        changes,
        replaces_full_config: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_assignments_without_echoing_secret_values() {
        assert!(validate_config_directive("scanner speed=fast", false).is_ok());
        let error = validate_config_directive("identity_openid client_secret", false)
            .expect_err("missing assignment must fail");
        assert!(!error.to_string().contains("super-secret"));
    }

    #[test]
    fn redacts_secret_fields_and_preserves_non_secret_values() {
        let redacted = redact_config_document(
            "identity_openid client_id=console client_secret=super-secret\nscanner speed=fast",
        );
        assert!(redacted.contains("client_id=\"console\""));
        assert!(redacted.contains("client_secret=\"*redacted*\""));
        assert!(redacted.contains("speed=\"fast\""));
        assert!(!redacted.contains("super-secret"));
    }

    #[test]
    fn set_diff_redacts_both_secret_sides() {
        let diff = config_mutation_diff(
            "identity_openid client_secret=old client_id=console",
            "identity_openid client_secret=new",
            false,
        )
        .expect("build set diff");
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].before.as_deref(), Some("*redacted*"));
        assert_eq!(diff.changes[0].after.as_deref(), Some("*redacted*"));
    }

    #[test]
    fn delete_diff_reports_only_present_keys() {
        let diff =
            config_mutation_diff("scanner speed=fast cycle=10", "scanner speed missing", true)
                .expect("build delete diff");
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].key, "speed");
        assert_eq!(diff.changes[0].after, None);
    }

    #[test]
    fn import_diff_reports_additions_changes_and_removals() {
        let diff = config_import_diff("scanner speed=fast cycle=10", "scanner speed=slow delay=1")
            .expect("build import diff");
        assert!(diff.replaces_full_config);
        assert_eq!(diff.changes.len(), 3);
    }

    #[test]
    fn redaction_preserves_quoted_value_semantics() {
        let redacted = redact_config_document(
            r#"identity_openid client_id="console app" client_secret="s3cr\"et""#,
        );
        assert!(redacted.contains(r#"client_id="console app""#));
        assert!(redacted.contains(r#"client_secret="*redacted*""#));
        assert!(validate_config_directive(&redacted, false).is_ok());
    }

    #[test]
    fn import_rejects_redacted_secret_placeholders() {
        let error = validate_config_import(
            r#"identity_openid client_id="console" client_secret="*redacted*""#,
        )
        .expect_err("redacted secrets must not be imported");
        assert_eq!(error.exit_code(), 2);
    }
}
