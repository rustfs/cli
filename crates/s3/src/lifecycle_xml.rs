use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::de::from_str as from_xml_str;
use quick_xml::events::Event;
use rc_core::{
    Error, LifecycleDelMarkerExpiration, LifecycleExpiration, LifecycleRule, LifecycleRuleStatus,
    LifecycleTransition, NoncurrentVersionExpiration, NoncurrentVersionTransition, Result,
};
use serde::Deserialize;

const S3_LIFECYCLE_XML_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleConfigurationXml {
    #[serde(rename = "Rule", default)]
    rules: Vec<LifecycleRuleXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleRuleXml {
    #[serde(rename = "ID")]
    id: Option<String>,
    status: Option<String>,
    #[serde(rename = "Prefix")]
    legacy_prefix: Option<String>,
    filter: Option<LifecycleFilterXml>,
    expiration: Option<LifecycleExpirationXml>,
    #[serde(rename = "Transition", default)]
    transitions: Vec<LifecycleTransitionXml>,
    noncurrent_version_expiration: Option<NoncurrentVersionExpirationXml>,
    #[serde(rename = "NoncurrentVersionTransition", default)]
    noncurrent_version_transitions: Vec<NoncurrentVersionTransitionXml>,
    abort_incomplete_multipart_upload: Option<AbortIncompleteMultipartUploadXml>,
    del_marker_expiration: Option<LifecycleDelMarkerExpirationXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleFilterXml {
    prefix: Option<String>,
    tag: Option<LifecycleTagXml>,
    object_size_greater_than: Option<i64>,
    object_size_less_than: Option<i64>,
    and: Option<LifecycleAndXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleAndXml {
    prefix: Option<String>,
    #[serde(rename = "Tag", default)]
    tags: Vec<LifecycleTagXml>,
    object_size_greater_than: Option<i64>,
    object_size_less_than: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleTagXml {
    key: Option<String>,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleExpirationXml {
    date: Option<String>,
    days: Option<i32>,
    expired_object_all_versions: Option<bool>,
    expired_object_delete_marker: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleTransitionXml {
    date: Option<String>,
    days: Option<i32>,
    storage_class: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NoncurrentVersionExpirationXml {
    noncurrent_days: Option<i32>,
    newer_noncurrent_versions: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NoncurrentVersionTransitionXml {
    noncurrent_days: Option<i32>,
    storage_class: Option<String>,
    newer_noncurrent_versions: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AbortIncompleteMultipartUploadXml {
    days_after_initiation: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LifecycleDelMarkerExpirationXml {
    days: Option<i32>,
}

pub(crate) fn parse_lifecycle_configuration_xml(body: &str) -> Result<Vec<LifecycleRule>> {
    validate_lifecycle_root(body)?;
    let config: LifecycleConfigurationXml = from_xml_str(body)
        .map_err(|error| Error::General(format!("parse bucket lifecycle xml: {error}")))?;

    config
        .rules
        .into_iter()
        .map(convert_lifecycle_rule)
        .collect()
}

pub(crate) fn validate_lifecycle_configuration_xml_response(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return Ok(());
    }
    validate_lifecycle_root(body)?;
    from_xml_str::<LifecycleConfigurationXml>(body)
        .map(|_| ())
        .map_err(|error| Error::General(format!("parse bucket lifecycle xml: {error}")))
}

fn convert_lifecycle_rule(rule: LifecycleRuleXml) -> Result<LifecycleRule> {
    let prefix = rule
        .filter
        .as_ref()
        .and_then(parse_filter_prefix)
        .or(rule.legacy_prefix);
    let tags = rule.filter.as_ref().and_then(parse_filter_tags);
    let (object_size_greater_than, object_size_less_than) = rule
        .filter
        .as_ref()
        .map(parse_filter_object_sizes)
        .unwrap_or((None, None));

    let (expiration, expired_object_delete_marker) = match rule.expiration {
        Some(expiration) => (
            Some(LifecycleExpiration {
                days: expiration.days,
                date: expiration.date,
                expired_object_all_versions: expiration.expired_object_all_versions,
            }),
            expiration
                .expired_object_delete_marker
                .filter(|value| *value),
        ),
        None => (None, None),
    };

    let transitions = rule
        .transitions
        .into_iter()
        .map(|transition| LifecycleTransition {
            days: transition.days,
            date: transition.date,
            storage_class: transition.storage_class.unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let transition = transitions.first().cloned();
    let additional_transitions = transitions.into_iter().skip(1).collect();

    let noncurrent_version_expiration =
        rule.noncurrent_version_expiration
            .map(|expiration| NoncurrentVersionExpiration {
                noncurrent_days: expiration.noncurrent_days.unwrap_or_default(),
                newer_noncurrent_versions: expiration.newer_noncurrent_versions,
            });

    let noncurrent_version_transitions = rule
        .noncurrent_version_transitions
        .into_iter()
        .map(|transition| NoncurrentVersionTransition {
            noncurrent_days: transition.noncurrent_days.unwrap_or_default(),
            storage_class: transition.storage_class.unwrap_or_default(),
            newer_noncurrent_versions: transition.newer_noncurrent_versions,
        })
        .collect::<Vec<_>>();
    let noncurrent_version_transition = noncurrent_version_transitions.first().cloned();
    let additional_noncurrent_version_transitions =
        noncurrent_version_transitions.into_iter().skip(1).collect();

    Ok(LifecycleRule {
        id: rule.id.unwrap_or_default(),
        status: parse_rule_status(rule.status.as_deref()),
        prefix,
        tags,
        object_size_greater_than,
        object_size_less_than,
        expiration,
        del_marker_expiration: rule.del_marker_expiration.map(|expiration| {
            LifecycleDelMarkerExpiration {
                days: expiration.days,
            }
        }),
        transition,
        transitions: additional_transitions,
        noncurrent_version_expiration,
        noncurrent_version_transition,
        noncurrent_version_transitions: additional_noncurrent_version_transitions,
        abort_incomplete_multipart_upload_days: rule
            .abort_incomplete_multipart_upload
            .and_then(|upload| upload.days_after_initiation),
        expired_object_delete_marker,
    })
}

fn validate_lifecycle_root(body: &str) -> Result<()> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                let root = element.local_name();
                if root.as_ref() == b"LifecycleConfiguration" {
                    return Ok(());
                }
                return Err(Error::General(format!(
                    "unexpected lifecycle root '{}', expected 'LifecycleConfiguration'",
                    String::from_utf8_lossy(root.as_ref())
                )));
            }
            Ok(Event::Eof) => {
                return Err(Error::General(
                    "parse bucket lifecycle xml: missing root element".to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(Error::General(format!(
                    "parse bucket lifecycle xml: {error}"
                )));
            }
        }
    }
}

fn parse_rule_status(status: Option<&str>) -> LifecycleRuleStatus {
    match status {
        Some(value) if value.eq_ignore_ascii_case("enabled") => LifecycleRuleStatus::Enabled,
        _ => LifecycleRuleStatus::Disabled,
    }
}

fn parse_filter_prefix(filter: &LifecycleFilterXml) -> Option<String> {
    filter
        .prefix
        .clone()
        .or_else(|| filter.and.as_ref().and_then(|and| and.prefix.clone()))
}

fn parse_filter_tags(filter: &LifecycleFilterXml) -> Option<HashMap<String, String>> {
    let mut tags = HashMap::new();
    if let Some(tag) = &filter.tag
        && let (Some(key), Some(value)) = (&tag.key, &tag.value)
    {
        tags.insert(key.clone(), value.clone());
    }
    if let Some(and) = &filter.and {
        for tag in &and.tags {
            if let (Some(key), Some(value)) = (&tag.key, &tag.value) {
                tags.insert(key.clone(), value.clone());
            }
        }
    }
    (!tags.is_empty()).then_some(tags)
}

fn parse_filter_object_sizes(filter: &LifecycleFilterXml) -> (Option<i64>, Option<i64>) {
    (
        filter.object_size_greater_than.or_else(|| {
            filter
                .and
                .as_ref()
                .and_then(|and| and.object_size_greater_than)
        }),
        filter.object_size_less_than.or_else(|| {
            filter
                .and
                .as_ref()
                .and_then(|and| and.object_size_less_than)
        }),
    )
}

pub(crate) fn build_lifecycle_configuration_xml(rules: &[LifecycleRule]) -> String {
    let mut xml =
        String::from(r#"<?xml version="1.0" encoding="UTF-8"?><LifecycleConfiguration xmlns=""#);
    xml.push_str(S3_LIFECYCLE_XML_NAMESPACE);
    xml.push_str(r#"">"#);

    for rule in rules {
        xml.push_str("<Rule>");

        if !rule.id.is_empty() {
            append_xml_element(&mut xml, "ID", &rule.id);
        }
        append_xml_element(
            &mut xml,
            "Status",
            match rule.status {
                LifecycleRuleStatus::Enabled => "Enabled",
                LifecycleRuleStatus::Disabled => "Disabled",
            },
        );
        append_filter_xml(
            &mut xml,
            rule.prefix.as_deref(),
            rule.tags.as_ref(),
            rule.object_size_greater_than,
            rule.object_size_less_than,
        );
        append_expiration_xml(
            &mut xml,
            rule.expiration.as_ref(),
            rule.expired_object_delete_marker,
        );

        append_transition_xml(&mut xml, rule.transition.as_ref());
        for transition in &rule.transitions {
            append_transition_xml(&mut xml, Some(transition));
        }

        if let Some(expiration) = &rule.noncurrent_version_expiration {
            xml.push_str("<NoncurrentVersionExpiration>");
            append_xml_element(
                &mut xml,
                "NoncurrentDays",
                &expiration.noncurrent_days.to_string(),
            );
            append_optional_i32(
                &mut xml,
                "NewerNoncurrentVersions",
                expiration.newer_noncurrent_versions,
            );
            xml.push_str("</NoncurrentVersionExpiration>");
        }

        append_noncurrent_version_transition_xml(
            &mut xml,
            rule.noncurrent_version_transition.as_ref(),
        );
        for transition in &rule.noncurrent_version_transitions {
            append_noncurrent_version_transition_xml(&mut xml, Some(transition));
        }

        if let Some(days) = rule.abort_incomplete_multipart_upload_days {
            xml.push_str("<AbortIncompleteMultipartUpload>");
            append_xml_element(&mut xml, "DaysAfterInitiation", &days.to_string());
            xml.push_str("</AbortIncompleteMultipartUpload>");
        }

        if let Some(expiration) = &rule.del_marker_expiration {
            xml.push_str("<DelMarkerExpiration>");
            append_optional_i32(&mut xml, "Days", expiration.days);
            xml.push_str("</DelMarkerExpiration>");
        }

        xml.push_str("</Rule>");
    }

    xml.push_str("</LifecycleConfiguration>");
    xml
}

fn append_filter_xml(
    xml: &mut String,
    prefix: Option<&str>,
    tags: Option<&HashMap<String, String>>,
    object_size_greater_than: Option<i64>,
    object_size_less_than: Option<i64>,
) {
    let tags = tags.filter(|tags| !tags.is_empty());
    let tag_count = tags.map_or(0, HashMap::len);
    let predicate_count = usize::from(prefix.is_some())
        + tag_count
        + usize::from(object_size_greater_than.is_some())
        + usize::from(object_size_less_than.is_some());
    if predicate_count == 0 {
        return;
    }

    xml.push_str("<Filter>");
    if predicate_count == 1 {
        if let Some(prefix) = prefix {
            append_xml_element(xml, "Prefix", prefix);
        } else if let Some(tags) = tags {
            if let Some((key, value)) = sorted_tags(tags).into_iter().next() {
                append_tag_xml(xml, key, value);
            }
        } else if let Some(value) = object_size_greater_than {
            append_xml_element(xml, "ObjectSizeGreaterThan", &value.to_string());
        } else if let Some(value) = object_size_less_than {
            append_xml_element(xml, "ObjectSizeLessThan", &value.to_string());
        }
    } else {
        xml.push_str("<And>");
        if let Some(prefix) = prefix {
            append_xml_element(xml, "Prefix", prefix);
        }
        if let Some(tags) = tags {
            for (key, value) in sorted_tags(tags) {
                append_tag_xml(xml, key, value);
            }
        }
        if let Some(value) = object_size_greater_than {
            append_xml_element(xml, "ObjectSizeGreaterThan", &value.to_string());
        }
        if let Some(value) = object_size_less_than {
            append_xml_element(xml, "ObjectSizeLessThan", &value.to_string());
        }
        xml.push_str("</And>");
    }
    xml.push_str("</Filter>");
}

fn append_transition_xml(xml: &mut String, transition: Option<&LifecycleTransition>) {
    let Some(transition) = transition else {
        return;
    };
    xml.push_str("<Transition>");
    append_optional_i32(xml, "Days", transition.days);
    append_optional_string(xml, "Date", transition.date.as_deref());
    append_xml_element(xml, "StorageClass", &transition.storage_class);
    xml.push_str("</Transition>");
}

fn append_noncurrent_version_transition_xml(
    xml: &mut String,
    transition: Option<&NoncurrentVersionTransition>,
) {
    let Some(transition) = transition else {
        return;
    };
    xml.push_str("<NoncurrentVersionTransition>");
    append_xml_element(
        xml,
        "NoncurrentDays",
        &transition.noncurrent_days.to_string(),
    );
    append_xml_element(xml, "StorageClass", &transition.storage_class);
    append_optional_i32(
        xml,
        "NewerNoncurrentVersions",
        transition.newer_noncurrent_versions,
    );
    xml.push_str("</NoncurrentVersionTransition>");
}

fn append_expiration_xml(
    xml: &mut String,
    expiration: Option<&LifecycleExpiration>,
    expired_object_delete_marker: Option<bool>,
) {
    let marker_enabled = expired_object_delete_marker == Some(true);
    let Some(expiration) = expiration else {
        if marker_enabled {
            xml.push_str("<Expiration>");
            append_xml_element(xml, "ExpiredObjectDeleteMarker", "true");
            xml.push_str("</Expiration>");
        }
        return;
    };

    xml.push_str("<Expiration>");
    append_optional_i32(xml, "Days", expiration.days);
    append_optional_string(xml, "Date", expiration.date.as_deref());
    append_optional_bool(
        xml,
        "ExpiredObjectAllVersions",
        expiration.expired_object_all_versions,
    );
    if marker_enabled {
        append_xml_element(xml, "ExpiredObjectDeleteMarker", "true");
    }
    xml.push_str("</Expiration>");
}

fn append_tag_xml(xml: &mut String, key: &str, value: &str) {
    xml.push_str("<Tag>");
    append_xml_element(xml, "Key", key);
    append_xml_element(xml, "Value", value);
    xml.push_str("</Tag>");
}

fn append_optional_i32(xml: &mut String, tag: &str, value: Option<i32>) {
    if let Some(value) = value {
        append_xml_element(xml, tag, &value.to_string());
    }
}

fn append_optional_string(xml: &mut String, tag: &str, value: Option<&str>) {
    if let Some(value) = value {
        append_xml_element(xml, tag, value);
    }
}

fn append_optional_bool(xml: &mut String, tag: &str, value: Option<bool>) {
    if let Some(value) = value {
        append_xml_element(xml, tag, if value { "true" } else { "false" });
    }
}

fn append_xml_element(xml: &mut String, tag: &str, value: &str) {
    xml.push('<');
    xml.push_str(tag);
    xml.push('>');
    xml.push_str(&xml_escape(value));
    xml.push_str("</");
    xml.push_str(tag);
    xml.push('>');
}

fn sorted_tags(tags: &HashMap<String, String>) -> Vec<(&str, &str)> {
    let mut pairs: Vec<(&str, &str)> = tags
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_xml_roundtrip_preserves_extension_fields_and_standard_actions() {
        let mut tags = HashMap::new();
        tags.insert("env".to_string(), "prod&blue".to_string());
        let rules = vec![LifecycleRule {
            id: "rule<&".to_string(),
            status: LifecycleRuleStatus::Enabled,
            prefix: Some("logs/".to_string()),
            tags: Some(tags),
            object_size_greater_than: None,
            object_size_less_than: None,
            expiration: Some(LifecycleExpiration {
                days: Some(1),
                date: None,
                expired_object_all_versions: Some(true),
            }),
            del_marker_expiration: Some(LifecycleDelMarkerExpiration { days: Some(2) }),
            transition: Some(LifecycleTransition {
                days: Some(30),
                date: None,
                storage_class: "WARM<&".to_string(),
            }),
            noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                noncurrent_days: 3,
                newer_noncurrent_versions: Some(1),
            }),
            noncurrent_version_transition: None,
            transitions: Vec::new(),
            noncurrent_version_transitions: Vec::new(),
            abort_incomplete_multipart_upload_days: Some(4),
            expired_object_delete_marker: None,
        }];

        let xml = build_lifecycle_configuration_xml(&rules);
        assert!(xml.contains("<ExpiredObjectAllVersions>true</ExpiredObjectAllVersions>"));
        assert!(xml.contains("<DelMarkerExpiration><Days>2</Days></DelMarkerExpiration>"));
        assert!(xml.contains("rule&lt;&amp;"));
        assert!(xml.contains("prod&amp;blue"));

        let parsed = parse_lifecycle_configuration_xml(&xml).expect("parse lifecycle XML");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "rule<&");
        assert_eq!(
            parsed[0]
                .expiration
                .as_ref()
                .and_then(|expiration| expiration.expired_object_all_versions),
            Some(true)
        );
        assert_eq!(
            parsed[0]
                .del_marker_expiration
                .as_ref()
                .and_then(|expiration| expiration.days),
            Some(2)
        );
        assert_eq!(
            parsed[0].tags.as_ref().and_then(|tags| tags.get("env")),
            Some(&"prod&blue".to_string())
        );
    }

    #[test]
    fn lifecycle_xml_parser_accepts_legacy_prefix_and_marker_expiration() {
        let xml = r#"
        <LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
          <Rule>
            <ID>legacy</ID>
            <Prefix>logs/</Prefix>
            <Status>Enabled</Status>
            <Expiration>
              <Days>1</Days>
              <ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker>
            </Expiration>
          </Rule>
        </LifecycleConfiguration>
        "#;

        let rules = parse_lifecycle_configuration_xml(xml).expect("parse legacy lifecycle XML");
        assert_eq!(rules[0].prefix.as_deref(), Some("logs/"));
        assert_eq!(rules[0].expired_object_delete_marker, Some(true));
    }

    #[test]
    fn lifecycle_xml_parser_rejects_error_and_unexpected_roots() {
        for xml in [
            "<Error><Code>InternalError</Code></Error>",
            "<Unexpected><Rule /></Unexpected>",
        ] {
            let result = parse_lifecycle_configuration_xml(xml);
            assert!(
                matches!(result, Err(Error::General(message)) if message.contains("unexpected lifecycle root")),
                "unexpected root should fail with a root-validation error: {xml}"
            );
        }
    }

    #[test]
    fn lifecycle_xml_response_validator_rejects_malformed_success_body() {
        assert!(
            validate_lifecycle_configuration_xml_response("<LifecycleConfiguration><Rule>")
                .is_err()
        );
        assert!(
            validate_lifecycle_configuration_xml_response("<LifecycleConfiguration />").is_ok()
        );
        assert!(validate_lifecycle_configuration_xml_response(" ").is_ok());
    }

    #[test]
    fn lifecycle_xml_roundtrip_preserves_size_filters_and_all_actions() {
        let rules = vec![LifecycleRule {
            id: "full-rule".to_string(),
            status: LifecycleRuleStatus::Enabled,
            prefix: Some("logs/".to_string()),
            tags: None,
            object_size_greater_than: Some(500),
            object_size_less_than: Some(64000),
            expiration: None,
            del_marker_expiration: None,
            transition: Some(LifecycleTransition {
                days: Some(30),
                date: None,
                storage_class: "WARM".to_string(),
            }),
            transitions: vec![LifecycleTransition {
                days: Some(60),
                date: None,
                storage_class: "COLD".to_string(),
            }],
            noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                noncurrent_days: 90,
                newer_noncurrent_versions: Some(2),
            }),
            noncurrent_version_transition: Some(NoncurrentVersionTransition {
                noncurrent_days: 90,
                storage_class: "WARM".to_string(),
                newer_noncurrent_versions: Some(2),
            }),
            noncurrent_version_transitions: vec![NoncurrentVersionTransition {
                noncurrent_days: 180,
                storage_class: "COLD".to_string(),
                newer_noncurrent_versions: Some(1),
            }],
            abort_incomplete_multipart_upload_days: None,
            expired_object_delete_marker: None,
        }];

        let xml = build_lifecycle_configuration_xml(&rules);
        assert!(xml.contains("<ObjectSizeGreaterThan>500</ObjectSizeGreaterThan>"));
        assert!(xml.contains("<ObjectSizeLessThan>64000</ObjectSizeLessThan>"));
        assert_eq!(xml.matches("<Transition>").count(), 2);
        assert_eq!(xml.matches("<NoncurrentVersionTransition>").count(), 2);
        assert!(xml.contains("<NewerNoncurrentVersions>2</NewerNoncurrentVersions>"));
        assert!(xml.contains("<NewerNoncurrentVersions>1</NewerNoncurrentVersions>"));

        let parsed = parse_lifecycle_configuration_xml(&xml).expect("parse full lifecycle XML");
        let rule = &parsed[0];
        assert_eq!(rule.object_size_greater_than, Some(500));
        assert_eq!(rule.object_size_less_than, Some(64000));
        assert_eq!(rule.transitions.len(), 1);
        assert_eq!(rule.noncurrent_version_transitions.len(), 1);
        assert_eq!(
            rule.noncurrent_version_transition
                .as_ref()
                .and_then(|transition| transition.newer_noncurrent_versions),
            Some(2)
        );
        assert_eq!(
            rule.noncurrent_version_transitions[0].newer_noncurrent_versions,
            Some(1)
        );

        let mut direct_filter = String::new();
        append_filter_xml(&mut direct_filter, None, None, Some(7), None);
        assert_eq!(
            direct_filter,
            "<Filter><ObjectSizeGreaterThan>7</ObjectSizeGreaterThan></Filter>"
        );
        let direct_xml = format!(
            "<LifecycleConfiguration><Rule><ID>direct-size</ID><Status>Enabled</Status>{direct_filter}</Rule></LifecycleConfiguration>"
        );
        let direct_rule = parse_lifecycle_configuration_xml(&direct_xml)
            .expect("direct size filter should parse")
            .into_iter()
            .next()
            .expect("direct size rule should be present");
        assert_eq!(direct_rule.object_size_greater_than, Some(7));
    }
}
