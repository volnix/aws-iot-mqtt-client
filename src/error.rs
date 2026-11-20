// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Typed application errors with a user-facing category, details, and a suggested action.

use std::fmt;

/// Broad error category shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// Missing/expired AWS credentials or insufficient IAM permissions.
    Credential,
    /// DNS failures, timeouts, unreachable hosts, dropped connections.
    Network,
    /// Invalid or unregistered x.509 certificate / private key, or an MQTT auth rejection.
    Authentication,
    /// Invalid user input (client ID, endpoint, topic, ...).
    Configuration,
}

impl ErrorCategory {
    pub fn label(self) -> &'static str {
        match self {
            ErrorCategory::Credential => "CREDENTIAL",
            ErrorCategory::Network => "NETWORK",
            ErrorCategory::Authentication => "AUTHENTICATION",
            ErrorCategory::Configuration => "CONFIGURATION",
        }
    }

    fn default_action(self) -> &'static str {
        match self {
            ErrorCategory::Credential => {
                "Check the selected AWS profile/credentials and that they grant the required IoT read permissions."
            }
            ErrorCategory::Network => "Check your network connection and try again.",
            ErrorCategory::Authentication => {
                "Verify your certificate and private key are valid, correctly paired, and the certificate is ACTIVE in AWS IoT."
            }
            ErrorCategory::Configuration => "Review your connection configuration parameters.",
        }
    }
}

/// Application error carrying everything the UI needs to explain a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    pub category: ErrorCategory,
    pub message: String,
    pub details: Option<String>,
    pub action: Option<String>,
}

impl AppError {
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            details: None,
            action: Some(category.default_action().to_owned()),
        }
    }

    pub fn credential(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Credential, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Network, message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Authentication, message)
    }

    pub fn configuration(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Configuration, message)
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// One-line summary: `[CATEGORY] message`.
    pub fn summary(&self) -> String {
        format!("[{}] {}", self.category.label(), self.message)
    }

    /// Multi-line description including details and suggested action.
    pub fn full(&self) -> String {
        let mut out = self.summary();
        if let Some(details) = &self.details {
            out.push_str("\nDetails: ");
            out.push_str(details);
        }
        if let Some(action) = &self.action {
            out.push_str("\nAction: ");
            out.push_str(action);
        }
        out
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

impl std::error::Error for AppError {}

/// Best-effort categorization of a free-form error message.
pub fn categorize_message(message: &str) -> ErrorCategory {
    let m = message.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| m.contains(n));

    if any(&[
        "credential",
        "access key",
        "secret key",
        "session token",
        "expiredtoken",
        "security token",
        "sso",
        "iam permission",
        "accessdenied",
        "not authorized to perform",
    ]) {
        ErrorCategory::Credential
    } else if any(&[
        "certificate",
        "cert",
        "private key",
        "handshake",
        "tls",
        "notauthorized",
        "not authorized",
        "unauthorized",
        "authentication",
        "bad user name or password",
        "badusernamepassword",
    ]) {
        ErrorCategory::Authentication
    } else if any(&[
        "network",
        "timeout",
        "timed out",
        "dns",
        "unreachable",
        "connection refused",
        "connection reset",
        "dispatch failure",
        "i/o",
        "io error",
        "failed to lookup",
    ]) {
        ErrorCategory::Network
    } else {
        ErrorCategory::Configuration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn full_includes_details_and_action() {
        let e = AppError::network("boom").with_details("dns failed");
        let s = e.full();
        assert!(s.starts_with("[NETWORK] boom"));
        assert!(s.contains("Details: dns failed"));
        assert!(s.contains("Action: Check your network"));
    }

    #[test]
    fn custom_action_overrides_default() {
        let e = AppError::credential("x").with_action("do y");
        assert_eq!(e.action.as_deref(), Some("do y"));
    }

    #[test]
    fn categorizes_known_messages() {
        assert_eq!(
            categorize_message("ExpiredToken: session expired"),
            ErrorCategory::Credential
        );
        assert_eq!(
            categorize_message("TLS handshake failed"),
            ErrorCategory::Authentication
        );
        assert_eq!(
            categorize_message("Connection timed out"),
            ErrorCategory::Network
        );
        assert_eq!(
            categorize_message("Client ID is too long"),
            ErrorCategory::Configuration
        );
    }

    proptest! {
        #[test]
        fn summary_always_prefixed_with_category(msg in ".*") {
            for e in [
                AppError::credential(msg.clone()),
                AppError::network(msg.clone()),
                AppError::authentication(msg.clone()),
                AppError::configuration(msg.clone()),
            ] {
                let prefix = format!("[{}] ", e.category.label());
                prop_assert!(e.summary().starts_with(&prefix));
                prop_assert!(e.full().starts_with(&e.summary()));
            }
        }

        #[test]
        fn categorize_never_panics(msg in ".*") {
            let _ = categorize_message(&msg);
        }
    }
}
