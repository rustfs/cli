//! Bucket CORS configuration types
//!
//! Domain types for S3 bucket CORS configuration and rules.

use serde::{Deserialize, Serialize};

/// Full CORS configuration for a bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsConfiguration {
    /// CORS rules
    pub rules: Vec<CorsRule>,
}

/// A single bucket CORS rule
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorsRule {
    /// Optional rule identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Allowed request origins
    pub allowed_origins: Vec<String>,

    /// Allowed HTTP methods
    pub allowed_methods: Vec<String>,

    /// Allowed request headers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_headers: Option<Vec<String>>,

    /// Exposed response headers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expose_headers: Option<Vec<String>>,

    /// Browser preflight cache duration in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cors_rule_serialization() {
        let rule = CorsRule {
            id: Some("web-app".to_string()),
            allowed_origins: vec!["https://app.example.com".to_string()],
            allowed_methods: vec!["GET".to_string(), "PUT".to_string()],
            allowed_headers: Some(vec![
                "Authorization".to_string(),
                "Content-Type".to_string(),
            ]),
            expose_headers: Some(vec!["ETag".to_string()]),
            max_age_seconds: Some(600),
        };

        let json = serde_json::to_string(&rule).expect("serialize rule");
        assert!(json.contains("allowedOrigins"));
        assert!(json.contains("maxAgeSeconds"));

        let decoded: CorsRule = serde_json::from_str(&json).expect("deserialize rule");
        assert_eq!(decoded.id.as_deref(), Some("web-app"));
        assert_eq!(
            decoded.allowed_origins,
            vec!["https://app.example.com".to_string()]
        );
        assert_eq!(
            decoded.allowed_methods,
            vec!["GET".to_string(), "PUT".to_string()]
        );
        assert_eq!(decoded.max_age_seconds, Some(600));
    }

    #[test]
    fn test_cors_configuration_serialization() {
        let config = CorsConfiguration {
            rules: vec![CorsRule {
                id: None,
                allowed_origins: vec!["*".to_string()],
                allowed_methods: vec!["GET".to_string()],
                allowed_headers: None,
                expose_headers: None,
                max_age_seconds: None,
            }],
        };

        let json = serde_json::to_string_pretty(&config).expect("serialize config");
        let decoded: CorsConfiguration = serde_json::from_str(&json).expect("deserialize config");
        assert_eq!(decoded.rules.len(), 1);
        assert_eq!(decoded.rules[0].allowed_origins, vec!["*".to_string()]);
        assert_eq!(decoded.rules[0].allowed_methods, vec!["GET".to_string()]);
    }
}
