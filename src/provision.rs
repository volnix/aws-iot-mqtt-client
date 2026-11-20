// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Creating a device certificate for a Thing (or a bare client ID), with an existing or new
//! IoT policy.
//!
//! Unlike the rest of the app, this module **writes** to the AWS account. It only runs when the
//! user explicitly confirms in the "Create certificate" dialog. It creates resources and
//! attaches them; it never detaches, deactivates, or deletes anything, including on failure.
//! Partial results are reported so the user can finish or clean up.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::aws::Session;
use crate::error::AppError;

pub const CERT_FILE: &str = "certificate.pem.crt";
pub const KEY_FILE: &str = "private.pem.key";
pub const PUBLIC_KEY_FILE: &str = "public.pem.key";
pub const MAX_POLICIES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyChoice {
    Existing(String),
    New { name: String, document: String },
}

impl PolicyChoice {
    pub fn name(&self) -> &str {
        match self {
            PolicyChoice::Existing(n) | PolicyChoice::New { name: n, .. } => n,
        }
    }
}

/// Who the certificate is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The certificate is attached to this Thing, and the client ID must equal its name.
    Thing(String),
    /// No Thing: the certificate is only authorized (via policy) to connect as this client ID.
    ClientId(String),
}

impl Identity {
    pub fn name(&self) -> &str {
        match self {
            Identity::Thing(n) | Identity::ClientId(n) => n,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Identity::Thing(n) => format!("Thing {n}"),
            Identity::ClientId(n) => format!("client ID {n}"),
        }
    }

    /// Folder name under the storage root.
    fn folder(&self) -> String {
        match self {
            Identity::Thing(n) => n.clone(),
            Identity::ClientId(n) => format!("client-{n}"),
        }
    }
}

/// Where policy ARNs point: partition, region, and account (`*` if unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArnScope {
    pub partition: String,
    pub region: String,
    pub account: String,
}

impl ArnScope {
    pub fn new(partition: &str, region: &str, account: Option<&str>) -> Self {
        Self {
            partition: partition.to_owned(),
            region: region.to_owned(),
            account: account.filter(|a| !a.is_empty()).unwrap_or("*").to_owned(),
        }
    }

    fn prefix(&self) -> String {
        format!(
            "arn:{}:iot:{}:{}",
            self.partition, self.region, self.account
        )
    }
}

#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub identity: Identity,
    pub policy: PolicyChoice,
    /// Root folder for saved credentials (normally [`default_storage_root`]).
    pub storage_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedCredentials {
    pub certificate_id: String,
    pub certificate_arn: String,
    pub directory: PathBuf,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Steps completed (in order), the saved files if the certificate was created, and an error if
/// a step failed.
#[derive(Debug, Clone)]
pub struct ProvisionOutcome {
    pub completed: Vec<String>,
    pub saved: Option<SavedCredentials>,
    pub error: Option<AppError>,
}

impl ProvisionOutcome {
    pub fn is_success(&self) -> bool {
        self.error.is_none() && self.saved.is_some()
    }
}

/// `<OS cache dir>/aws-iot-mqtt-client/certificates`, falling back to the temp dir.
/// macOS: `~/Library/Caches/...`, Linux: `~/.cache/...`, Windows: `%LOCALAPPDATA%\...`.
pub fn default_storage_root() -> PathBuf {
    crate::paths::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("aws-iot-mqtt-client")
        .join("certificates")
}

/// Default policy document.
///
/// For a Thing, this follows the AWS IoT thing-policy best practice: the client may connect
/// only with a client ID equal to its Thing name (`${iot:Connection.Thing.ThingName}`), and only
/// when the certificate is attached to that Thing (`iot:Connection.Thing.IsAttached`). See
/// <https://docs.aws.amazon.com/iot/latest/developerguide/thing-policy-examples.html>.
///
/// For a bare client ID, connect is limited to that literal client ID, since there is no Thing
/// for the policy variables to resolve against.
///
/// Both allow publish, receive, and subscribe on every topic.
pub fn default_policy_document(identity: &Identity, scope: &ArnScope) -> String {
    let prefix = scope.prefix();
    let connect = match identity {
        Identity::Thing(_) => format!(
            r#"    {{
      "Effect": "Allow",
      "Action": "iot:Connect",
      "Resource": "{prefix}:client/${{iot:Connection.Thing.ThingName}}",
      "Condition": {{
        "Bool": {{
          "iot:Connection.Thing.IsAttached": "true"
        }}
      }}
    }}"#
        ),
        Identity::ClientId(client_id) => format!(
            r#"    {{
      "Effect": "Allow",
      "Action": "iot:Connect",
      "Resource": "{prefix}:client/{}"
    }}"#,
            json_escape(client_id)
        ),
    };
    format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
{connect},
    {{
      "Effect": "Allow",
      "Action": ["iot:Publish", "iot:Receive"],
      "Resource": "{prefix}:topic/*"
    }},
    {{
      "Effect": "Allow",
      "Action": "iot:Subscribe",
      "Resource": "{prefix}:topicfilter/*"
    }}
  ]
}}
"#
    )
}

/// Account ID from an IoT ARN such as a Thing ARN (fallback when STS is unavailable).
pub fn account_from_arn(arn: &str) -> Option<&str> {
    parse_arn(arn).map(|(_, _, account)| account)
}

pub fn default_policy_name(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| if is_policy_name_char(c) { c } else { '-' })
        .collect();
    let mut name = format!("{base}-policy");
    name.truncate(128);
    name
}

/// IoT policy names: 1-128 chars of `[A-Za-z0-9_+=,.@-]`.
pub fn validate_policy_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("Policy name must be 1-128 characters".into());
    }
    if !name.chars().all(is_policy_name_char) {
        return Err("Policy name may only contain letters, digits, and _ + = , . @ -".into());
    }
    Ok(())
}

/// Checks the document is a JSON object with a `Statement`. AWS validates the full grammar.
pub fn validate_policy_document(document: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(document).map_err(|e| format!("Invalid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or("Policy document must be a JSON object")?;
    match obj.get("Statement") {
        Some(serde_json::Value::Array(a)) if !a.is_empty() => Ok(()),
        Some(serde_json::Value::Object(_)) => Ok(()),
        _ => Err("Policy document needs a non-empty \"Statement\"".into()),
    }
}

fn is_policy_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_+=,.@-".contains(c)
}

fn json_escape(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_default();
    quoted[1..quoted.len() - 1].to_owned()
}

/// `arn:<partition>:iot:<region>:<account>:...` → `(partition, region, account)`.
fn parse_arn(arn: &str) -> Option<(&str, &str, &str)> {
    let mut parts = arn.splitn(6, ':');
    let (Some("arn"), Some(partition), Some("iot"), Some(region), Some(account)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    (!partition.is_empty() && !region.is_empty() && !account.is_empty())
        .then_some((partition, region, account))
}

/// Writes the certificate and keys to `<root>/<thing or client-id>/<certificate id>/`. On Unix
/// the folder is `0700` and the private key is `0600`.
pub fn save_credentials(
    root: &Path,
    identity: &Identity,
    certificate_id: &str,
    certificate_arn: &str,
    certificate_pem: &str,
    private_key_pem: &str,
    public_key_pem: Option<&str>,
) -> Result<SavedCredentials, AppError> {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let directory = root
        .join(safe(&identity.folder()))
        .join(safe(certificate_id));
    let io_err = |what: &str, e: std::io::Error| {
        AppError::configuration(format!("Could not save {what}"))
            .with_details(e.to_string())
            .with_action("Check that the app's cache folder is writable.")
    };

    fs::create_dir_all(&directory).map_err(|e| io_err("certificate folder", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("certificate folder", e))?;
    }

    let cert_path = directory.join(CERT_FILE);
    let key_path = directory.join(KEY_FILE);
    write_file(&key_path, private_key_pem, true).map_err(|e| io_err("private key", e))?;
    write_file(&cert_path, certificate_pem, false).map_err(|e| io_err("certificate", e))?;
    if let Some(pub_pem) = public_key_pem {
        write_file(&directory.join(PUBLIC_KEY_FILE), pub_pem, false)
            .map_err(|e| io_err("public key", e))?;
    }

    Ok(SavedCredentials {
        certificate_id: certificate_id.to_owned(),
        certificate_arn: certificate_arn.to_owned(),
        directory,
        cert_path,
        key_path,
    })
}

fn write_file(path: &Path, contents: &str, secret: bool) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if secret { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()
}

impl Session {
    /// `GetPolicy`: the default version's document for a policy (read-only).
    pub async fn get_policy_document(&self, name: &str) -> Result<String, AppError> {
        let out = self
            .call(
                "GetPolicy",
                format!("policyName={name}"),
                self.client.get_policy().policy_name(name).send(),
                |o| {
                    format!(
                        "defaultVersionId={} document=<{} bytes>",
                        o.default_version_id().unwrap_or("?"),
                        o.policy_document().map(str::len).unwrap_or(0)
                    )
                },
            )
            .await?;
        out.policy_document()
            .map(pretty_json)
            .ok_or_else(|| AppError::network(format!("GetPolicy returned no document for {name}")))
    }

    /// `ListPolicies`, following pagination up to [`MAX_POLICIES`]. Sorted by name.
    pub async fn list_policies(&self) -> Result<Vec<String>, AppError> {
        let mut names = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let out = self
                .call(
                    "ListPolicies",
                    format!(
                        "pageSize=250{}",
                        marker.as_ref().map(|_| " marker=…").unwrap_or("")
                    ),
                    self.client
                        .list_policies()
                        .page_size(250)
                        .set_marker(marker.take())
                        .send(),
                    |o| format!("{} policies", o.policies().len()),
                )
                .await?;
            names.extend(
                out.policies()
                    .iter()
                    .filter_map(|p| p.policy_name().map(str::to_owned)),
            );
            marker = out
                .next_marker()
                .filter(|m| !m.is_empty())
                .map(str::to_owned);
            if marker.is_none() || names.len() >= MAX_POLICIES {
                break;
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    /// Steps, in order:
    /// 1. `CreatePolicy` (new policy only). Runs first so a bad document or a name collision
    ///    fails before a key pair exists.
    /// 2. `CreateKeysAndCertificate` (ACTIVE), then save the files locally right away. AWS
    ///    returns the private key only once.
    /// 3. `AttachPolicy` (policy → certificate)
    /// 4. `AttachThingPrincipal` (certificate → Thing), for a Thing identity only
    pub async fn provision_certificate(&self, req: &ProvisionRequest) -> ProvisionOutcome {
        let mut outcome = ProvisionOutcome {
            completed: Vec::new(),
            saved: None,
            error: None,
        };
        if let Err(e) = self.provision_steps(req, &mut outcome).await {
            outcome.error = Some(describe_partial_failure(e, &outcome));
        }
        outcome
    }

    async fn provision_steps(
        &self,
        req: &ProvisionRequest,
        out: &mut ProvisionOutcome,
    ) -> Result<(), AppError> {
        let policy_name = req.policy.name().to_owned();
        validate_policy_name(&policy_name).map_err(AppError::configuration)?;

        if let PolicyChoice::New { name, document } = &req.policy {
            validate_policy_document(document).map_err(AppError::configuration)?;
            self.call(
                "CreatePolicy",
                format!(
                    "policyName={name} policyDocument=<{} bytes>",
                    document.len()
                ),
                self.client
                    .create_policy()
                    .policy_name(name)
                    .policy_document(document)
                    .send(),
                |o| format!("policyArn={}", o.policy_arn().unwrap_or("?")),
            )
            .await?;
            out.completed.push(format!("Created policy {name}"));
        }

        // The response contains the private key: the log summary must only show the IDs.
        let created = self
            .call(
                "CreateKeysAndCertificate",
                "setAsActive=true",
                self.client
                    .create_keys_and_certificate()
                    .set_as_active(true)
                    .send(),
                |o| {
                    format!(
                        "certificateId={} (private key received, not logged)",
                        o.certificate_id().unwrap_or("?")
                    )
                },
            )
            .await?;
        let missing = |what: &str| {
            AppError::network(format!(
                "CreateKeysAndCertificate response is missing {what}"
            ))
        };
        let cert_id = created
            .certificate_id()
            .ok_or_else(|| missing("the certificate ID"))?;
        let cert_arn = created
            .certificate_arn()
            .ok_or_else(|| missing("the certificate ARN"))?;
        let cert_pem = created
            .certificate_pem()
            .ok_or_else(|| missing("the certificate PEM"))?;
        let key_pair = created.key_pair().ok_or_else(|| missing("the key pair"))?;
        let private_key = key_pair
            .private_key()
            .ok_or_else(|| missing("the private key"))?;
        out.completed
            .push(format!("Created ACTIVE certificate {cert_id}"));

        let saved = save_credentials(
            &req.storage_root,
            &req.identity,
            cert_id,
            cert_arn,
            cert_pem,
            private_key,
            key_pair.public_key(),
        )?;
        out.completed.push(format!(
            "Saved certificate and private key to {}",
            saved.directory.display()
        ));
        out.saved = Some(saved);

        self.call(
            "AttachPolicy",
            format!("policyName={policy_name} target={cert_arn}"),
            self.client
                .attach_policy()
                .policy_name(&policy_name)
                .target(cert_arn)
                .send(),
            |_| String::new(),
        )
        .await?;
        out.completed
            .push(format!("Attached policy {policy_name} to the certificate"));

        if let Identity::Thing(thing) = &req.identity {
            self.call(
                "AttachThingPrincipal",
                format!("thingName={thing} principal={cert_arn}"),
                self.client
                    .attach_thing_principal()
                    .thing_name(thing)
                    .principal(cert_arn)
                    .send(),
                |_| String::new(),
            )
            .await?;
            out.completed
                .push(format!("Attached the certificate to Thing {thing}"));
        }
        Ok(())
    }
}

/// Re-indents JSON for display; returns the input unchanged if it isn't valid JSON.
pub fn pretty_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| text.to_owned())
}

fn describe_partial_failure(err: AppError, outcome: &ProvisionOutcome) -> AppError {
    if outcome.completed.is_empty() {
        return err;
    }
    let mut details = err.details.clone().unwrap_or_default();
    details.push_str("\n\nAlready done in your account (not rolled back):\n");
    for step in &outcome.completed {
        details.push_str("  ✔ ");
        details.push_str(step);
        details.push('\n');
    }
    let action = if outcome.saved.is_some() {
        "The certificate and key are saved and loaded. Fix the problem (usually an IAM permission) and finish the remaining attachment in the AWS IoT console, or deactivate and delete the certificate if you don't need it."
    } else {
        "Fix the problem and try again. Delete any policy created above if you don't want to keep it."
    };
    AppError {
        details: Some(details),
        action: Some(action.into()),
        ..err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ArnScope {
        ArnScope::new("aws", "eu-west-1", Some("123456789012"))
    }

    #[test]
    fn thing_policy_follows_best_practice() {
        let doc = default_policy_document(&Identity::Thing("sensor-1".into()), &scope());
        validate_policy_document(&doc).unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        let stmts = v["Statement"].as_array().unwrap();
        assert_eq!(stmts[0]["Action"], "iot:Connect");
        assert_eq!(
            stmts[0]["Resource"],
            "arn:aws:iot:eu-west-1:123456789012:client/${iot:Connection.Thing.ThingName}"
        );
        assert_eq!(
            stmts[0]["Condition"]["Bool"]["iot:Connection.Thing.IsAttached"],
            "true"
        );
        assert_eq!(
            stmts[1]["Action"],
            serde_json::json!(["iot:Publish", "iot:Receive"])
        );
        assert_eq!(
            stmts[1]["Resource"],
            "arn:aws:iot:eu-west-1:123456789012:topic/*"
        );
        assert_eq!(stmts[2]["Action"], "iot:Subscribe");
        assert_eq!(
            stmts[2]["Resource"],
            "arn:aws:iot:eu-west-1:123456789012:topicfilter/*"
        );
    }

    #[test]
    fn client_id_policy_uses_literal_client_id() {
        let doc = default_policy_document(&Identity::ClientId("my-app_1".into()), &scope());
        validate_policy_document(&doc).unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        let connect = &v["Statement"][0];
        assert_eq!(
            connect["Resource"],
            "arn:aws:iot:eu-west-1:123456789012:client/my-app_1"
        );
        assert!(connect.get("Condition").is_none());
    }

    #[test]
    fn unknown_account_uses_wildcard_and_partition_is_respected() {
        let s = ArnScope::new("aws-cn", "cn-north-1", None);
        let doc = default_policy_document(&Identity::Thing("t".into()), &s);
        assert!(doc.contains("arn:aws-cn:iot:cn-north-1:*:client/"));
        validate_policy_document(&doc).unwrap();
    }

    #[test]
    fn account_from_thing_arn() {
        assert_eq!(
            account_from_arn("arn:aws:iot:us-east-1:111122223333:thing/t"),
            Some("111122223333")
        );
        assert_eq!(account_from_arn("nope"), None);
    }

    #[test]
    fn pretty_json_reindents_or_passes_through() {
        assert_eq!(pretty_json(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty_json("not json"), "not json");
    }

    #[test]
    fn policy_name_rules() {
        assert!(validate_policy_name("my-thing_policy.v1@x").is_ok());
        assert!(validate_policy_name("").is_err());
        assert!(validate_policy_name("has space").is_err());
        assert!(validate_policy_name(&"a".repeat(129)).is_err());
        assert_eq!(default_policy_name("a:b"), "a-b-policy");
        assert!(validate_policy_name(&default_policy_name(&"x".repeat(128))).is_ok());
    }

    #[test]
    fn policy_document_rules() {
        assert!(validate_policy_document("not json").is_err());
        assert!(validate_policy_document("[]").is_err());
        assert!(validate_policy_document(r#"{"Version":"2012-10-17"}"#).is_err());
        assert!(validate_policy_document(r#"{"Statement":[]}"#).is_err());
        assert!(validate_policy_document(r#"{"Statement":{"Effect":"Allow"}}"#).is_ok());
    }

    #[test]
    fn saves_credentials_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let saved = save_credentials(
            dir.path(),
            &Identity::Thing("my/thing".into()),
            "abc123",
            "arn:x",
            "CERT",
            "KEY",
            Some("PUB"),
        )
        .unwrap();
        assert_eq!(saved.directory, dir.path().join("my_thing").join("abc123"));
        assert_eq!(fs::read_to_string(&saved.cert_path).unwrap(), "CERT");
        assert_eq!(fs::read_to_string(&saved.key_path).unwrap(), "KEY");
        assert_eq!(
            fs::read_to_string(saved.directory.join(PUBLIC_KEY_FILE)).unwrap(),
            "PUB"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&saved.key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&saved.directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn client_id_credentials_get_their_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let saved = save_credentials(
            dir.path(),
            &Identity::ClientId("app-1".into()),
            "def456",
            "arn:x",
            "CERT",
            "KEY",
            None,
        )
        .unwrap();
        assert_eq!(
            saved.directory,
            dir.path().join("client-app-1").join("def456")
        );
        assert!(!saved.directory.join(PUBLIC_KEY_FILE).exists());
    }

    #[test]
    fn partial_failure_lists_completed_steps() {
        let outcome = ProvisionOutcome {
            completed: vec!["Created policy p".into()],
            saved: None,
            error: None,
        };
        let e = describe_partial_failure(AppError::credential("AttachPolicy failed"), &outcome);
        assert!(e.details.unwrap().contains("✔ Created policy p"));
        assert_eq!(e.message, "AttachPolicy failed");
    }
}
