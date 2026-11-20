// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Input validation for connection parameters and MQTT topics.

/// Maximum client ID length accepted by AWS IoT Core.
pub const MAX_CLIENT_ID_LEN: usize = 128;
/// Maximum topic length (bytes) accepted by AWS IoT Core.
pub const MAX_TOPIC_LEN: usize = 256;

/// Client IDs: 1-128 chars of `[A-Za-z0-9_:-]`. Colons are allowed because they're valid in
/// AWS IoT Thing names, which the app uses as the default client ID.
pub fn validate_client_id(client_id: &str) -> Result<(), String> {
    let len = client_id.chars().count();
    if len == 0 || len > MAX_CLIENT_ID_LEN {
        return Err(format!(
            "Client ID must be 1-{MAX_CLIENT_ID_LEN} characters"
        ));
    }
    if !client_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
    {
        return Err(
            "Client ID may only contain letters, digits, hyphens, underscores, and colons".into(),
        );
    }
    Ok(())
}

/// Endpoint must be a bare hostname (no scheme, port, or path).
pub fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    let e = endpoint.trim();
    if e.is_empty() {
        return Err("Endpoint is required".into());
    }
    if e.contains("://") || e.contains('/') || e.contains(':') {
        return Err("Endpoint must be a hostname only (no scheme, port, or path)".into());
    }
    if !e
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err("Endpoint contains invalid characters".into());
    }
    Ok(())
}

/// Topic name for PUBLISH: non-empty, at most 256 bytes, no wildcards.
pub fn validate_publish_topic(topic: &str) -> Result<(), String> {
    check_topic_common(topic)?;
    if topic.contains(['+', '#']) {
        return Err("Publish topics cannot contain wildcards (+ or #)".into());
    }
    Ok(())
}

/// Topic filter for SUBSCRIBE: `+` must occupy a whole level, `#` must be the whole last level.
pub fn validate_topic_filter(filter: &str) -> Result<(), String> {
    check_topic_common(filter)?;
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        if level.contains('#') && (*level != "#" || i != levels.len() - 1) {
            return Err("'#' must be the entire last level of the filter".into());
        }
        if level.contains('+') && *level != "+" {
            return Err("'+' must occupy an entire topic level".into());
        }
    }
    Ok(())
}

fn check_topic_common(topic: &str) -> Result<(), String> {
    if topic.is_empty() {
        return Err("Topic is required".into());
    }
    if topic.len() > MAX_TOPIC_LEN {
        return Err(format!("Topic must be at most {MAX_TOPIC_LEN} bytes"));
    }
    if topic.contains('\0') {
        return Err("Topic cannot contain NUL characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn client_id_rules() {
        assert!(validate_client_id("my-thing_01").is_ok());
        assert!(validate_client_id("thing:with:colons").is_ok());
        assert!(validate_client_id("").is_err());
        assert!(validate_client_id(&"a".repeat(129)).is_err());
        assert!(validate_client_id(&"a".repeat(128)).is_ok());
        assert!(validate_client_id("has space").is_err());
        assert!(validate_client_id("emoji🙂").is_err());
    }

    #[test]
    fn endpoint_rules() {
        assert!(validate_endpoint("abc123-ats.iot.us-east-1.amazonaws.com").is_ok());
        assert!(validate_endpoint("mqtts://abc.com").is_err());
        assert!(validate_endpoint("abc.com:8883").is_err());
        assert!(validate_endpoint("").is_err());
    }

    #[test]
    fn topic_rules() {
        assert!(validate_publish_topic("dt/thing/telemetry").is_ok());
        assert!(validate_publish_topic("a/+/b").is_err());
        assert!(validate_topic_filter("a/+/b").is_ok());
        assert!(validate_topic_filter("a/#").is_ok());
        assert!(validate_topic_filter("#").is_ok());
        assert!(validate_topic_filter("a/#/b").is_err());
        assert!(validate_topic_filter("a/b#").is_err());
        assert!(validate_topic_filter("a/b+").is_err());
        assert!(validate_topic_filter("").is_err());
    }

    proptest! {
        #[test]
        fn valid_client_ids_accepted(id in "[A-Za-z0-9_:-]{1,128}") {
            prop_assert!(validate_client_id(&id).is_ok());
        }

        #[test]
        fn client_ids_with_invalid_char_rejected(
            prefix in "[A-Za-z0-9]{0,10}",
            bad in "[^A-Za-z0-9_:-]",
            suffix in "[A-Za-z0-9]{0,10}",
        ) {
            let id = format!("{prefix}{bad}{suffix}");
            prop_assert!(validate_client_id(&id).is_err());
        }

        #[test]
        fn valid_publish_topics_are_valid_filters(t in "[a-z0-9]{1,8}(/[a-z0-9]{1,8}){0,5}") {
            prop_assert!(validate_publish_topic(&t).is_ok());
            prop_assert!(validate_topic_filter(&t).is_ok());
        }
    }
}
