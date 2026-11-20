// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Offline AWS IoT Core policy simulator for MQTT operations.
//!
//! Given a policy document, an MQTT operation, and the connection context, this decides whether
//! AWS IoT Core would accept or reject the operation, and explains why. Nothing is sent to AWS.
//!
//! Operations follow the MQTT specification rather than every AWS IoT action:
//!
//! | MQTT operation                  | IoT policy action(s)                 | Resource              |
//! |---------------------------------|--------------------------------------|-----------------------|
//! | CONNECT                         | `iot:Connect`                        | `client/<client ID>`  |
//! | PUBLISH (client to broker)      | `iot:Publish` (+ `iot:RetainPublish` if retained) | `topic/<topic>` |
//! | SUBSCRIBE                       | `iot:Subscribe`                      | `topicfilter/<filter>`|
//! | PUBLISH (broker to client)      | `iot:Receive`                        | `topic/<topic>`       |
//!
//! UNSUBSCRIBE isn't listed because AWS IoT policies have no action for it.
//!
//! Evaluation follows the AWS IoT / IAM model: an explicit `Deny` wins, otherwise an `Allow`
//! is required, otherwise the request is implicitly denied. `*` and `?` are wildcards in
//! `Action` and `Resource`; MQTT's `+` and `#` are literal characters in policies. When the
//! simulator can't evaluate something (an unsupported condition operator), the result is marked
//! as not certain instead of guessing.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde_json::Value;

use crate::validators::{validate_publish_topic, validate_topic_filter};

/// AWS IoT policy documents are limited to 2048 characters, excluding whitespace.
pub const MAX_POLICY_CHARS: usize = 2048;
pub const POLICY_VERSION: &str = "2012-10-17";

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// An MQTT operation to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MqttOperation {
    Connect,
    Publish {
        topic: String,
        retain: bool,
    },
    Subscribe {
        filter: String,
    },
    /// A PUBLISH the broker delivers to this client (checked as `iot:Receive`).
    Receive {
        topic: String,
    },
}

impl MqttOperation {
    pub fn label(&self) -> &'static str {
        match self {
            MqttOperation::Connect => "CONNECT",
            MqttOperation::Publish { .. } => "PUBLISH",
            MqttOperation::Subscribe { .. } => "SUBSCRIBE",
            MqttOperation::Receive { .. } => "PUBLISH from broker",
        }
    }
}

/// A Thing in the simulated connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThingContext {
    pub name: String,
    /// The connecting certificate is attached to this Thing.
    pub attached: bool,
    /// The certificate is attached exclusively, so the Thing is known even if the client ID
    /// differs from its name.
    pub exclusive: bool,
    pub thing_type: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

/// Everything about the connection that a policy can refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimContext {
    pub partition: String,
    pub region: String,
    pub account: String,
    pub client_id: String,
    pub thing: Option<ThingContext>,
    /// Additional variables and condition keys, e.g. `aws:SourceIp` or
    /// `iot:Certificate.Subject.CommonName`.
    pub extra: BTreeMap<String, String>,
}

impl SimContext {
    /// Whether the Thing is identified for this connection. Per AWS, thing policy variables
    /// need the client ID to match the Thing name unless the attachment is exclusive.
    fn thing_identified(&self) -> Option<&ThingContext> {
        self.thing
            .as_ref()
            .filter(|t| t.attached && (t.exclusive || t.name == self.client_id))
    }

    /// Values for policy variables and condition keys.
    pub fn values(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        values.insert("iot:ClientId".to_owned(), self.client_id.clone());
        values.insert(
            "iot:Connection.Thing.IsAttached".to_owned(),
            self.thing_identified().is_some().to_string(),
        );
        if let Some(thing) = self.thing_identified() {
            values.insert(
                "iot:Connection.Thing.ThingName".to_owned(),
                thing.name.clone(),
            );
            if let Some(kind) = &thing.thing_type {
                values.insert(
                    "iot:Connection.Thing.ThingTypeName".to_owned(),
                    kind.clone(),
                );
            }
            for (k, v) in &thing.attributes {
                values.insert(format!("iot:Connection.Thing.Attributes[{k}]"), v.clone());
            }
        }
        for (k, v) in &self.extra {
            values.insert(k.clone(), v.clone());
        }
        values
    }

    fn arn(&self, resource: &str) -> String {
        format!(
            "arn:{}:iot:{}:{}:{resource}",
            self.partition, self.region, self.account
        )
    }
}

/// One (action, resource) pair the broker authorizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub action: &'static str,
    pub resource: String,
}

/// The authorization checks AWS IoT performs for an operation.
pub fn checks_for(op: &MqttOperation, ctx: &SimContext) -> Result<Vec<Check>, String> {
    if ctx.client_id.is_empty() {
        return Err("Enter the client ID the device connects with".into());
    }
    if ctx.client_id.chars().count() > 128 {
        return Err("Client IDs can be at most 128 characters".into());
    }
    for (name, value) in [("Region", &ctx.region), ("Account", &ctx.account)] {
        if value.trim().is_empty() {
            return Err(format!("{name} is required to build the resource ARN"));
        }
    }
    let checks = match op {
        MqttOperation::Connect => vec![Check {
            action: "iot:Connect",
            resource: ctx.arn(&format!("client/{}", ctx.client_id)),
        }],
        MqttOperation::Publish { topic, retain } => {
            validate_publish_topic(topic)?;
            let resource = ctx.arn(&format!("topic/{topic}"));
            let mut checks = vec![Check {
                action: "iot:Publish",
                resource: resource.clone(),
            }];
            if *retain {
                checks.push(Check {
                    action: "iot:RetainPublish",
                    resource,
                });
            }
            checks
        }
        MqttOperation::Subscribe { filter } => {
            validate_topic_filter(filter)?;
            vec![Check {
                action: "iot:Subscribe",
                resource: ctx.arn(&format!("topicfilter/{filter}")),
            }]
        }
        MqttOperation::Receive { topic } => {
            validate_publish_topic(topic)
                .map_err(|e| format!("{e} (messages are delivered on a concrete topic)"))?;
            vec![Check {
                action: "iot:Receive",
                resource: ctx.arn(&format!("topic/{topic}")),
            }]
        }
    };
    Ok(checks)
}

// ---------------------------------------------------------------------------------------------
// Policy model and parsing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
}

/// `Action`/`Resource` (match any listed) or `NotAction`/`NotResource` (match none listed).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListMatch {
    Any(Vec<String>),
    NoneOf(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConditionClause {
    /// Full operator as written, e.g. `StringLikeIfExists`.
    operator: String,
    key: String,
    values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub index: usize,
    pub sid: Option<String>,
    pub effect: Effect,
    actions: ListMatch,
    resources: ListMatch,
    conditions: Vec<ConditionClause>,
}

impl Statement {
    /// "#2 (AllowPublish)" or "#2".
    pub fn label(&self) -> String {
        match &self.sid {
            Some(sid) => format!("#{} ({sid})", self.index + 1),
            None => format!("#{}", self.index + 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub statements: Vec<Statement>,
    /// Problems that don't stop evaluation but are worth fixing.
    pub warnings: Vec<String>,
}

/// Parses an IoT policy document. Errors carry the JSON position when relevant.
pub fn parse_policy(json: &str) -> Result<Policy, String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("Invalid JSON: {e}"))?;
    let doc = value
        .as_object()
        .ok_or("The policy document must be a JSON object")?;
    let mut warnings = Vec::new();

    match doc.get("Version").and_then(Value::as_str) {
        Some(POLICY_VERSION) => {}
        Some(other) => warnings.push(format!(
            "Version is {other:?}; AWS IoT policies use \"{POLICY_VERSION}\"."
        )),
        None => warnings.push(format!(
            "Add \"Version\": \"{POLICY_VERSION}\" so policy variables are supported."
        )),
    }
    let compact = json.chars().filter(|c| !c.is_whitespace()).count();
    if compact > MAX_POLICY_CHARS {
        warnings.push(format!(
            "{compact} characters (excluding whitespace); AWS IoT allows at most {MAX_POLICY_CHARS}."
        ));
    }
    for key in doc.keys() {
        if !matches!(key.as_str(), "Version" | "Statement" | "Id") {
            warnings.push(format!("Unknown top-level element {key:?} is ignored."));
        }
    }

    let raw_statements: Vec<&Value> = match doc.get("Statement") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(obj @ Value::Object(_)) => vec![obj],
        Some(_) => return Err("\"Statement\" must be an object or an array".into()),
        None => return Err("The policy has no \"Statement\"".into()),
    };
    if raw_statements.is_empty() {
        return Err("\"Statement\" is empty".into());
    }

    let statements = raw_statements
        .into_iter()
        .enumerate()
        .map(|(index, raw)| parse_statement(index, raw, &mut warnings))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Policy {
        statements,
        warnings,
    })
}

fn parse_statement(
    index: usize,
    raw: &Value,
    warnings: &mut Vec<String>,
) -> Result<Statement, String> {
    let at = |msg: &str| format!("Statement #{}: {msg}", index + 1);
    let obj = raw.as_object().ok_or_else(|| at("must be an object"))?;

    if obj.contains_key("Principal") || obj.contains_key("NotPrincipal") {
        return Err(at(
            "AWS IoT policies can't contain \"Principal\"; the policy applies to the principal it's attached to",
        ));
    }
    let effect = match obj.get("Effect").and_then(Value::as_str) {
        Some("Allow") => Effect::Allow,
        Some("Deny") => Effect::Deny,
        Some(other) => {
            return Err(at(&format!(
                "\"Effect\" must be \"Allow\" or \"Deny\", not {other:?}"
            )))
        }
        None => return Err(at("missing \"Effect\"")),
    };
    let actions = list_match(obj, "Action", "NotAction").map_err(|e| at(&e))?;
    let resources = list_match(obj, "Resource", "NotResource").map_err(|e| at(&e))?;

    let mut conditions = Vec::new();
    if let Some(cond) = obj.get("Condition") {
        let operators = cond
            .as_object()
            .ok_or_else(|| at("\"Condition\" must be an object"))?;
        for (operator, keys) in operators {
            let keys = keys
                .as_object()
                .ok_or_else(|| at(&format!("condition {operator:?} must map keys to values")))?;
            for (key, values) in keys {
                conditions.push(ConditionClause {
                    operator: operator.clone(),
                    key: key.clone(),
                    values: string_list(values)
                        .map_err(|e| at(&format!("condition {operator} {key}: {e}")))?,
                });
            }
        }
    }
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "Sid" | "Effect" | "Action" | "NotAction" | "Resource" | "NotResource" | "Condition"
        ) {
            warnings.push(at(&format!("unknown element {key:?} is ignored.")));
        }
    }

    Ok(Statement {
        index,
        sid: obj.get("Sid").and_then(Value::as_str).map(str::to_owned),
        effect,
        actions,
        resources,
        conditions,
    })
}

fn list_match(
    obj: &serde_json::Map<String, Value>,
    positive: &str,
    negative: &str,
) -> Result<ListMatch, String> {
    match (obj.get(positive), obj.get(negative)) {
        (Some(_), Some(_)) => Err(format!(
            "use either \"{positive}\" or \"{negative}\", not both"
        )),
        (Some(v), None) => Ok(ListMatch::Any(string_list(v)?)),
        (None, Some(v)) => Ok(ListMatch::NoneOf(string_list(v)?)),
        (None, None) => Err(format!("missing \"{positive}\"")),
    }
}

fn string_list(value: &Value) -> Result<Vec<String>, String> {
    let scalar = |v: &Value| match v {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err("values must be strings".to_owned()),
    };
    match value {
        Value::Array(items) if items.is_empty() => Err("must not be empty".into()),
        Value::Array(items) => items.iter().map(scalar).collect(),
        other => Ok(vec![scalar(other)?]),
    }
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

/// A compiled pattern: literal characters plus `*` (any run) and `?` (one character).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Lit(char),
    Star,
    One,
}

/// Replaces `${...}` policy variables in a template. Variable values are literal (a `*` in a
/// client ID is not a wildcard); `${*}`, `${?}`, and `${$}` produce those literal characters.
/// Returns the name of the first variable that has no value.
fn compile(
    template: &str,
    values: &BTreeMap<String, String>,
    wildcards: bool,
) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        push_literal_part(&rest[..start], wildcards, &mut tokens);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unterminated "${" is ordinary text.
            push_literal_part(&rest[start..], wildcards, &mut tokens);
            return Ok(tokens);
        };
        let name = &after[..end];
        match name {
            "*" | "?" | "$" => tokens.push(Token::Lit(name.chars().next().unwrap_or('$'))),
            _ => match values.get(name) {
                Some(value) => tokens.extend(value.chars().map(Token::Lit)),
                None => return Err(name.to_owned()),
            },
        }
        rest = &after[end + 1..];
    }
    push_literal_part(rest, wildcards, &mut tokens);
    Ok(tokens)
}

fn push_literal_part(text: &str, wildcards: bool, tokens: &mut Vec<Token>) {
    tokens.extend(text.chars().map(|c| match c {
        '*' if wildcards => Token::Star,
        '?' if wildcards => Token::One,
        c => Token::Lit(c),
    }));
}

/// Glob match with backtracking over `*`. `fold` makes literals case-insensitive.
fn matches(pattern: &[Token], subject: &str, fold: bool) -> bool {
    let subject: Vec<char> = subject.chars().collect();
    let eq = |a: char, b: char| {
        if fold {
            a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };
    let (mut p, mut s) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while s < subject.len() {
        match pattern.get(p) {
            Some(Token::Star) => {
                backtrack = Some((p, s));
                p += 1;
            }
            Some(Token::One) => {
                p += 1;
                s += 1;
            }
            Some(Token::Lit(c)) if eq(*c, subject[s]) => {
                p += 1;
                s += 1;
            }
            _ => match backtrack {
                Some((star_p, star_s)) => {
                    p = star_p + 1;
                    s = star_s + 1;
                    backtrack = Some((star_p, star_s + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|t| *t == Token::Star)
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The statement applies to this request.
    Applies,
    /// The statement doesn't apply.
    DoesNotApply,
    /// The simulator can't tell (e.g. an unsupported condition operator).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementTrace {
    pub statement: String,
    pub effect: Effect,
    pub outcome: Outcome,
    /// Why, in plain words.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub check: Check,
    pub allowed: bool,
    pub certain: bool,
    pub summary: String,
    pub trace: Vec<StatementTrace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simulation {
    pub decision: Decision,
    /// False when an unsupported element could change the decision.
    pub certain: bool,
    pub summary: String,
    pub checks: Vec<CheckResult>,
    /// Policy variables a statement needed that the context doesn't provide.
    pub unresolved: Vec<String>,
}

/// Simulates `op` against `policy` in `ctx`.
pub fn simulate(
    policy: &Policy,
    op: &MqttOperation,
    ctx: &SimContext,
) -> Result<Simulation, String> {
    let values = ctx.values();
    let mut unresolved = Vec::new();
    let checks: Vec<CheckResult> = checks_for(op, ctx)?
        .into_iter()
        .map(|check| evaluate_check(policy, check, &values, &mut unresolved))
        .collect();
    unresolved.sort();
    unresolved.dedup();

    let decision = if checks.iter().all(|c| c.allowed) {
        Decision::Accept
    } else {
        Decision::Reject
    };
    let certain = checks.iter().all(|c| c.certain);
    let summary = match decision {
        Decision::Accept => format!("AWS IoT would accept this {}.", op.label()),
        Decision::Reject => {
            let failed = checks
                .iter()
                .find(|c| !c.allowed)
                .map(|c| c.summary.clone());
            format!(
                "AWS IoT would reject this {}. {}",
                op.label(),
                failed.unwrap_or_default()
            )
        }
    };
    Ok(Simulation {
        decision,
        certain,
        summary: summary.trim().to_owned(),
        checks,
        unresolved,
    })
}

fn evaluate_check(
    policy: &Policy,
    check: Check,
    values: &BTreeMap<String, String>,
    unresolved: &mut Vec<String>,
) -> CheckResult {
    let trace: Vec<StatementTrace> = policy
        .statements
        .iter()
        .map(|st| {
            let (outcome, reason) = evaluate_statement(st, &check, values, unresolved);
            StatementTrace {
                statement: st.label(),
                effect: st.effect,
                outcome,
                reason,
            }
        })
        .collect();

    let first = |effect: Effect, outcome: Outcome| {
        trace
            .iter()
            .find(|t| t.effect == effect && t.outcome == outcome)
    };
    let (allowed, certain, summary) = if let Some(t) = first(Effect::Deny, Outcome::Applies) {
        (
            false,
            true,
            format!("Explicitly denied by statement {}.", t.statement),
        )
    } else if let Some(t) = first(Effect::Allow, Outcome::Applies) {
        match first(Effect::Deny, Outcome::Unknown) {
            Some(d) => (
                true,
                false,
                format!(
                    "Allowed by statement {}, unless Deny statement {} applies (it can't be fully evaluated).",
                    t.statement, d.statement
                ),
            ),
            None => (true, true, format!("Allowed by statement {}.", t.statement)),
        }
    } else if let Some(t) = first(Effect::Allow, Outcome::Unknown) {
        (
            false,
            false,
            format!(
                "No statement definitely allows {}; statement {} might (it can't be fully evaluated).",
                check.action, t.statement
            ),
        )
    } else {
        (
            false,
            true,
            format!(
                "No statement allows {} on this resource, so it's implicitly denied.",
                check.action
            ),
        )
    };

    CheckResult {
        check,
        allowed,
        certain,
        summary,
        trace,
    }
}

fn evaluate_statement(
    st: &Statement,
    check: &Check,
    values: &BTreeMap<String, String>,
    unresolved: &mut Vec<String>,
) -> (Outcome, String) {
    // Action: case-insensitive, wildcards allowed, no variables.
    let action_hit = |patterns: &[String]| {
        patterns.iter().any(|p| {
            let tokens: Vec<Token> = p
                .chars()
                .map(|c| match c {
                    '*' => Token::Star,
                    '?' => Token::One,
                    c => Token::Lit(c),
                })
                .collect();
            matches(&tokens, check.action, true)
        })
    };
    let action_ok = match &st.actions {
        ListMatch::Any(list) => action_hit(list),
        ListMatch::NoneOf(list) => !action_hit(list),
    };
    if !action_ok {
        return (
            Outcome::DoesNotApply,
            format!("Action doesn't cover {}.", check.action),
        );
    }

    // Resource: case-sensitive, wildcards and variables. A pattern whose variables can't be
    // resolved matches nothing (IAM behavior).
    let mut missing = Vec::new();
    let mut resource_hit = |patterns: &[String]| {
        patterns.iter().any(|p| match compile(p, values, true) {
            Ok(tokens) => matches(&tokens, &check.resource, false),
            Err(var) => {
                missing.push(var);
                false
            }
        })
    };
    let resource_ok = match &st.resources {
        ListMatch::Any(list) => resource_hit(list),
        ListMatch::NoneOf(list) => !resource_hit(list),
    };
    unresolved.extend(missing.iter().cloned());
    if !resource_ok {
        let reason = if missing.is_empty() {
            "Resource doesn't match.".to_owned()
        } else {
            format!(
                "Resource doesn't match: ${{{}}} has no value for this connection.",
                missing.join("}, ${")
            )
        };
        return (Outcome::DoesNotApply, reason);
    }

    // Conditions: every clause must hold.
    let mut unknown = None;
    for clause in &st.conditions {
        match evaluate_condition(clause, values) {
            Ok(true) => {}
            Ok(false) => {
                return (
                    Outcome::DoesNotApply,
                    format!("Condition {} on {} is false.", clause.operator, clause.key),
                )
            }
            Err(ConditionError::Unresolved(var)) => {
                unresolved.push(var.clone());
                return (
                    Outcome::DoesNotApply,
                    format!(
                        "Condition {} on {} uses ${{{var}}}, which has no value.",
                        clause.operator, clause.key
                    ),
                );
            }
            Err(ConditionError::Unsupported(why)) => {
                unknown.get_or_insert(why);
            }
        }
    }
    match unknown {
        Some(why) => (Outcome::Unknown, why),
        None if st.conditions.is_empty() => (Outcome::Applies, "Action and resource match.".into()),
        None => (
            Outcome::Applies,
            "Action, resource, and conditions match.".into(),
        ),
    }
}

enum ConditionError {
    Unresolved(String),
    Unsupported(String),
}

fn evaluate_condition(
    clause: &ConditionClause,
    values: &BTreeMap<String, String>,
) -> Result<bool, ConditionError> {
    let mut op = clause.operator.as_str();
    if op.starts_with("ForAnyValue:") || op.starts_with("ForAllValues:") {
        return Err(ConditionError::Unsupported(format!(
            "Set operator {op} isn't supported by the simulator."
        )));
    }
    let if_exists = op.ends_with("IfExists") && op != "IfExists";
    if if_exists {
        op = &op[..op.len() - "IfExists".len()];
    }
    let actual = values.get(&clause.key);

    if op == "Null" {
        let want_absent = clause.values.iter().any(|v| v.eq_ignore_ascii_case("true"));
        return Ok(actual.is_none() == want_absent);
    }

    let negated = matches!(
        op,
        "StringNotEquals"
            | "StringNotEqualsIgnoreCase"
            | "StringNotLike"
            | "ArnNotEquals"
            | "ArnNotLike"
            | "NotIpAddress"
    );
    let Some(actual) = actual else {
        // IAM: a missing key fails positive operators and satisfies negated ones;
        // "...IfExists" is satisfied when the key is missing.
        return Ok(if_exists || negated);
    };

    let resolve = |v: &String, wildcards: bool| {
        compile(v, values, wildcards).map_err(ConditionError::Unresolved)
    };
    let literal = |tokens: Vec<Token>| -> String {
        tokens
            .into_iter()
            .map(|t| match t {
                Token::Lit(c) => c,
                Token::Star => '*',
                Token::One => '?',
            })
            .collect()
    };

    let any = |test: &dyn Fn(&String) -> Result<bool, ConditionError>| {
        for v in &clause.values {
            if test(v)? {
                return Ok(true);
            }
        }
        Ok(false)
    };

    let positive = match op {
        "StringEquals" | "StringNotEquals" | "ArnEquals" | "ArnNotEquals" => {
            any(&|v| Ok(literal(resolve(v, false)?) == *actual))?
        }
        "StringEqualsIgnoreCase" | "StringNotEqualsIgnoreCase" => {
            any(&|v| Ok(literal(resolve(v, false)?).to_lowercase() == actual.to_lowercase()))?
        }
        "StringLike" | "StringNotLike" | "ArnLike" | "ArnNotLike" => {
            any(&|v| Ok(matches(&resolve(v, true)?, actual, false)))?
        }
        "Bool" => any(&|v| Ok(v.eq_ignore_ascii_case(actual)))?,
        "IpAddress" | "NotIpAddress" => {
            let ip: IpAddr = actual.parse().map_err(|_| {
                ConditionError::Unsupported(format!("{} is not an IP address.", clause.key))
            })?;
            any(&|v| {
                cidr_contains(v, ip).ok_or_else(|| {
                    ConditionError::Unsupported(format!("{v:?} is not an IP address or CIDR."))
                })
            })?
        }
        other => {
            return Err(ConditionError::Unsupported(format!(
                "Condition operator {other} isn't supported by the simulator."
            )))
        }
    };
    Ok(positive != negated)
}

/// Whether `ip` falls inside `cidr` ("10.0.0.0/8", "2001:db8::/32", or a single address).
fn cidr_contains(cidr: &str, ip: IpAddr) -> Option<bool> {
    let (addr, bits) = match cidr.split_once('/') {
        Some((a, b)) => (a.parse::<IpAddr>().ok()?, Some(b.parse::<u32>().ok()?)),
        None => (cidr.parse::<IpAddr>().ok()?, None),
    };
    match (addr, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let bits = bits.unwrap_or(32);
            if bits > 32 {
                return None;
            }
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            Some(u32::from(net) & mask == u32::from(ip) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let bits = bits.unwrap_or(128);
            if bits > 128 {
                return None;
            }
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            Some(u128::from(net) & mask == u128::from(ip) & mask)
        }
        _ => Some(false),
    }
}

/// Parses `key=value` lines (blank lines and `#` comments ignored).
pub fn parse_key_values(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("Line {}: expected key=value", i + 1))?;
        let k = k.trim();
        if k.is_empty() {
            return Err(format!("Line {}: key is empty", i + 1));
        }
        map.insert(k.to_owned(), v.trim().to_owned());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ARN: &str = "arn:aws:iot:us-east-1:123456789012";

    fn ctx(client: &str) -> SimContext {
        SimContext {
            partition: "aws".into(),
            region: "us-east-1".into(),
            account: "123456789012".into(),
            client_id: client.into(),
            thing: None,
            extra: BTreeMap::new(),
        }
    }

    fn thing(name: &str, attached: bool) -> ThingContext {
        ThingContext {
            name: name.into(),
            attached,
            ..Default::default()
        }
    }

    fn policy(statements: &str) -> Policy {
        parse_policy(&format!(
            r#"{{"Version":"2012-10-17","Statement":[{statements}]}}"#
        ))
        .unwrap()
    }

    fn decide(p: &Policy, op: MqttOperation, c: &SimContext) -> Decision {
        simulate(p, &op, c).unwrap().decision
    }

    fn publish(topic: &str) -> MqttOperation {
        MqttOperation::Publish {
            topic: topic.into(),
            retain: false,
        }
    }

    #[test]
    fn implicit_deny_when_nothing_allows() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/a"}}"#
        ));
        let sim = simulate(&p, &MqttOperation::Connect, &ctx("c1")).unwrap();
        assert_eq!(sim.decision, Decision::Reject);
        assert!(sim.certain);
        assert!(sim.checks[0].summary.contains("implicitly denied"));
    }

    #[test]
    fn explicit_deny_beats_allow() {
        // AWS docs example: allow department/*, deny department/admins.
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/department/*"}},
               {{"Sid":"NoAdmins","Effect":"Deny","Action":"iot:Publish","Resource":"{ARN}:topic/department/admins"}}"#
        ));
        assert_eq!(
            decide(&p, publish("department/eng"), &ctx("c")),
            Decision::Accept
        );
        let sim = simulate(&p, &publish("department/admins"), &ctx("c")).unwrap();
        assert_eq!(sim.decision, Decision::Reject);
        assert!(sim.summary.contains("NoAdmins"));
    }

    #[test]
    fn publish_and_receive_use_topic_subscribe_uses_topicfilter() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":["iot:Publish","iot:Receive"],"Resource":"{ARN}:topic/t"}},
               {{"Effect":"Allow","Action":"iot:Subscribe","Resource":"{ARN}:topicfilter/t"}}"#
        ));
        let c = ctx("c");
        assert_eq!(decide(&p, publish("t"), &c), Decision::Accept);
        assert_eq!(
            decide(&p, MqttOperation::Receive { topic: "t".into() }, &c),
            Decision::Accept
        );
        assert_eq!(
            decide(&p, MqttOperation::Subscribe { filter: "t".into() }, &c),
            Decision::Accept
        );
        // A topic resource doesn't authorize SUBSCRIBE.
        let only_topic = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:*","Resource":"{ARN}:topic/t"}}"#
        ));
        assert_eq!(
            decide(
                &only_topic,
                MqttOperation::Subscribe { filter: "t".into() },
                &c
            ),
            Decision::Reject
        );
    }

    #[test]
    fn mqtt_wildcards_are_literal_in_policies() {
        // AWS docs: topicfilter/location/# allows subscribing to "location/#" only.
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Subscribe","Resource":"{ARN}:topicfilter/location/#"}}"#
        ));
        let c = ctx("c");
        let sub = |f: &str| MqttOperation::Subscribe { filter: f.into() };
        assert_eq!(decide(&p, sub("location/#"), &c), Decision::Accept);
        assert_eq!(decide(&p, sub("location/Seattle"), &c), Decision::Reject);
    }

    #[test]
    fn policy_wildcards_star_and_question_mark() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Pub*","Resource":"{ARN}:topic/some/?????/topic"}}"#
        ));
        let c = ctx("c");
        assert_eq!(
            decide(&p, publish("some/abcde/topic"), &c),
            Decision::Accept
        );
        assert_eq!(decide(&p, publish("some/abcd/topic"), &c), Decision::Reject);
        // `*` crosses topic levels.
        let star = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/a/*"}}"#
        ));
        assert_eq!(decide(&star, publish("a/b/c/d"), &c), Decision::Accept);
    }

    #[test]
    fn actions_are_case_insensitive_resources_are_not() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"IOT:PUBLISH","Resource":"{ARN}:topic/Data"}}"#
        ));
        let c = ctx("c");
        assert_eq!(decide(&p, publish("Data"), &c), Decision::Accept);
        assert_eq!(decide(&p, publish("data"), &c), Decision::Reject);
    }

    #[test]
    fn retained_publish_also_needs_retain_publish() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/t"}}"#
        ));
        let c = ctx("c");
        let retained = MqttOperation::Publish {
            topic: "t".into(),
            retain: true,
        };
        let sim = simulate(&p, &retained, &c).unwrap();
        assert_eq!(sim.decision, Decision::Reject);
        assert_eq!(sim.checks.len(), 2);
        assert!(sim.checks[0].allowed && !sim.checks[1].allowed);
        let both = policy(&format!(
            r#"{{"Effect":"Allow","Action":["iot:Publish","iot:RetainPublish"],"Resource":"{ARN}:topic/t"}}"#
        ));
        assert_eq!(decide(&both, retained, &c), Decision::Accept);
    }

    #[test]
    fn thing_best_practice_policy() {
        // The AWS docs "registered devices" Connect statement.
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Connect","Resource":"{ARN}:client/${{iot:Connection.Thing.ThingName}}",
                "Condition":{{"Bool":{{"iot:Connection.Thing.IsAttached":"true"}}}}}}"#
        ));
        let mut c = ctx("sensor-1");
        c.thing = Some(thing("sensor-1", true));
        assert_eq!(decide(&p, MqttOperation::Connect, &c), Decision::Accept);

        // Not attached: the variable has no value, so the statement can't match.
        c.thing = Some(thing("sensor-1", false));
        let sim = simulate(&p, &MqttOperation::Connect, &c).unwrap();
        assert_eq!(sim.decision, Decision::Reject);
        assert_eq!(sim.unresolved, ["iot:Connection.Thing.ThingName"]);

        // Client ID differs from the Thing name (non-exclusive attachment): not identified.
        let mut other = ctx("someone-else");
        other.thing = Some(thing("sensor-1", true));
        assert_eq!(decide(&p, MqttOperation::Connect, &other), Decision::Reject);

        // Exclusive attachment identifies the Thing regardless of client ID; the resource is
        // still client/<thing name>, so a different client ID is rejected.
        let mut exclusive = other.clone();
        exclusive.thing.as_mut().unwrap().exclusive = true;
        assert_eq!(
            decide(&p, MqttOperation::Connect, &exclusive),
            Decision::Reject
        );
    }

    #[test]
    fn thing_attributes_and_client_id_variables() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/sensor/${{iot:Connection.Thing.Attributes[version]}}"}},
               {{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/clients/${{iot:ClientId}}/*"}}"#
        ));
        let mut c = ctx("dev-1");
        let mut t = thing("dev-1", true);
        t.attributes.insert("version".into(), "v1".into());
        c.thing = Some(t);
        assert_eq!(decide(&p, publish("sensor/v1"), &c), Decision::Accept);
        assert_eq!(decide(&p, publish("sensor/v2"), &c), Decision::Reject);
        assert_eq!(
            decide(&p, publish("clients/dev-1/status"), &c),
            Decision::Accept
        );
        assert_eq!(
            decide(&p, publish("clients/dev-2/status"), &c),
            Decision::Reject
        );
    }

    #[test]
    fn variable_values_are_literal_not_wildcards() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/${{iot:ClientId}}"}}"#
        ));
        // A client ID of "*" must not match every topic.
        assert_eq!(decide(&p, publish("anything"), &ctx("*")), Decision::Reject);
        assert_eq!(decide(&p, publish("*"), &ctx("*")), Decision::Accept);
    }

    #[test]
    fn special_character_variables() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Publish","Resource":"{ARN}:topic/a${{*}}b"}}"#
        ));
        let c = ctx("c");
        assert_eq!(decide(&p, publish("a*b"), &c), Decision::Accept);
        assert_eq!(decide(&p, publish("axyb"), &c), Decision::Reject);
    }

    #[test]
    fn not_resource_and_not_action() {
        // AWS docs: receive everything except topic_prefix/restricted/*.
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Receive","NotResource":"{ARN}:topic/topic_prefix/restricted/*"}},
               {{"Effect":"Allow","NotAction":"iot:Connect","Resource":"{ARN}:topic/open"}}"#
        ));
        let c = ctx("c");
        let recv = |t: &str| MqttOperation::Receive { topic: t.into() };
        assert_eq!(
            decide(&p, recv("topic_prefix/topic1"), &c),
            Decision::Accept
        );
        assert_eq!(
            decide(&p, recv("topic_prefix/restricted/x"), &c),
            Decision::Reject
        );
        assert_eq!(decide(&p, publish("open"), &c), Decision::Accept);
    }

    #[test]
    fn condition_operators() {
        let stmt = |cond: &str| {
            policy(&format!(
                r#"{{"Effect":"Allow","Action":"iot:Connect","Resource":"{ARN}:client/*","Condition":{cond}}}"#
            ))
        };
        let mut c = ctx("prod-device-7");
        c.extra.insert("aws:SourceIp".into(), "10.1.2.3".into());

        let cases = [
            (
                r#"{"StringLike":{"iot:ClientId":"prod-*"}}"#,
                Decision::Accept,
            ),
            (
                r#"{"StringLike":{"iot:ClientId":["dev-*","test-*"]}}"#,
                Decision::Reject,
            ),
            (
                r#"{"StringEquals":{"iot:ClientId":"prod-device-7"}}"#,
                Decision::Accept,
            ),
            (
                r#"{"StringEqualsIgnoreCase":{"iot:ClientId":"PROD-DEVICE-7"}}"#,
                Decision::Accept,
            ),
            (
                r#"{"StringNotEquals":{"iot:ClientId":"prod-device-7"}}"#,
                Decision::Reject,
            ),
            (
                r#"{"IpAddress":{"aws:SourceIp":"10.0.0.0/8"}}"#,
                Decision::Accept,
            ),
            (
                r#"{"NotIpAddress":{"aws:SourceIp":"10.0.0.0/8"}}"#,
                Decision::Reject,
            ),
            (
                r#"{"Null":{"iot:Connection.Thing.ThingName":"true"}}"#,
                Decision::Accept,
            ),
            (
                r#"{"Bool":{"iot:Connection.Thing.IsAttached":"true"}}"#,
                Decision::Reject,
            ),
            // Missing key: positive fails, IfExists passes, negated passes.
            (
                r#"{"StringEquals":{"iot:DomainName":"x"}}"#,
                Decision::Reject,
            ),
            (
                r#"{"StringEqualsIfExists":{"iot:DomainName":"x"}}"#,
                Decision::Accept,
            ),
            (
                r#"{"StringNotEquals":{"iot:DomainName":"x"}}"#,
                Decision::Accept,
            ),
            // Multiple keys under one operator are ANDed.
            (
                r#"{"StringLike":{"iot:ClientId":"prod-*","aws:SourceIp":"10.*"}}"#,
                Decision::Accept,
            ),
        ];
        for (cond, want) in cases {
            assert_eq!(
                decide(&stmt(cond), MqttOperation::Connect, &c),
                want,
                "{cond}"
            );
        }
    }

    #[test]
    fn condition_values_can_use_variables() {
        // AWS docs: client ID must end with the envType attribute.
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Connect","Resource":"{ARN}:client/*",
                "Condition":{{"StringLike":{{"iot:ClientId":"*${{iot:Connection.Thing.Attributes[envType]}}"}}}}}}"#
        ));
        let mut c = ctx("sensor-prod");
        let mut t = thing("sensor-prod", true);
        t.attributes.insert("envType".into(), "prod".into());
        c.thing = Some(t);
        assert_eq!(decide(&p, MqttOperation::Connect, &c), Decision::Accept);
        c.thing
            .as_mut()
            .unwrap()
            .attributes
            .insert("envType".into(), "dev".into());
        assert_eq!(decide(&p, MqttOperation::Connect, &c), Decision::Reject);
    }

    #[test]
    fn unsupported_operators_make_the_result_uncertain() {
        let p = policy(&format!(
            r#"{{"Effect":"Allow","Action":"iot:Connect","Resource":"{ARN}:client/*"}},
               {{"Effect":"Deny","Action":"iot:Connect","Resource":"{ARN}:client/*","Condition":{{"DateGreaterThan":{{"aws:CurrentTime":"2020-01-01T00:00:00Z"}}}}}}"#
        ));
        let mut c = ctx("c");
        c.extra
            .insert("aws:CurrentTime".into(), "2026-01-01T00:00:00Z".into());
        let sim = simulate(&p, &MqttOperation::Connect, &c).unwrap();
        assert_eq!(sim.decision, Decision::Accept);
        assert!(!sim.certain, "an unevaluated Deny could flip the result");
    }

    #[test]
    fn invalid_documents_explain_the_problem() {
        assert!(parse_policy("{").unwrap_err().contains("Invalid JSON"));
        assert!(parse_policy("[]").unwrap_err().contains("object"));
        assert!(parse_policy(r#"{"Statement":[]}"#)
            .unwrap_err()
            .contains("empty"));
        let principal =
            r#"{"Statement":{"Effect":"Allow","Principal":"*","Action":"iot:*","Resource":"*"}}"#;
        assert!(parse_policy(principal).unwrap_err().contains("Principal"));
        let bad_effect = r#"{"Statement":{"Effect":"Maybe","Action":"iot:*","Resource":"*"}}"#;
        assert!(parse_policy(bad_effect).unwrap_err().contains("Effect"));
        let both = r#"{"Statement":{"Effect":"Allow","Action":"iot:*","NotAction":"iot:Connect","Resource":"*"}}"#;
        assert!(parse_policy(both).unwrap_err().contains("not both"));
        let no_version = r#"{"Statement":{"Effect":"Allow","Action":"iot:*","Resource":"*"}}"#;
        assert!(parse_policy(no_version).unwrap().warnings[0].contains("Version"));
    }

    #[test]
    fn requests_follow_mqtt_rules() {
        let p = policy(r#"{"Effect":"Allow","Action":"iot:*","Resource":"*"}"#);
        let c = ctx("c");
        assert!(
            simulate(&p, &publish("a/+/b"), &c).is_err(),
            "no wildcards in PUBLISH"
        );
        assert!(simulate(
            &p,
            &MqttOperation::Subscribe {
                filter: "a/#/b".into()
            },
            &c
        )
        .is_err());
        assert!(simulate(&p, &MqttOperation::Connect, &ctx("")).is_err());
        let mut no_region = ctx("c");
        no_region.region.clear();
        assert!(simulate(&p, &MqttOperation::Connect, &no_region).is_err());
    }

    #[test]
    fn key_value_lines() {
        let map = parse_key_values("# comment\nlocation = Seattle\n\nversion=v1\n").unwrap();
        assert_eq!(map["location"], "Seattle");
        assert_eq!(map["version"], "v1");
        assert!(parse_key_values("oops").is_err());
        assert!(parse_key_values("=x").is_err());
    }

    #[test]
    fn cidr_matching() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(cidr_contains("10.0.0.0/8", ip("10.200.1.1")), Some(true));
        assert_eq!(cidr_contains("10.0.0.0/8", ip("11.0.0.1")), Some(false));
        assert_eq!(cidr_contains("192.168.1.5", ip("192.168.1.5")), Some(true));
        assert_eq!(cidr_contains("0.0.0.0/0", ip("8.8.8.8")), Some(true));
        assert_eq!(
            cidr_contains("2001:db8::/32", ip("2001:db8::1")),
            Some(true)
        );
        assert_eq!(cidr_contains("10.0.0.0/8", ip("2001:db8::1")), Some(false));
        assert_eq!(cidr_contains("10.0.0.0/33", ip("10.0.0.1")), None);
    }

    proptest! {
        /// A literal pattern (no wildcards) matches exactly itself.
        #[test]
        fn literal_patterns_match_only_themselves(a in "[a-z/]{0,12}", b in "[a-z/]{0,12}") {
            let tokens = compile(&a, &BTreeMap::new(), true).unwrap();
            prop_assert_eq!(matches(&tokens, &b, false), a == b);
        }

        /// `prefix*` matches every string with that prefix.
        #[test]
        fn star_suffix_matches_any_continuation(prefix in "[a-z/]{0,8}", rest in "[a-z/]{0,8}") {
            let tokens = compile(&format!("{prefix}*"), &BTreeMap::new(), true).unwrap();
            let subject = format!("{prefix}{rest}");
            prop_assert!(matches(&tokens, &subject, false));
        }

        /// Matching never panics, whatever the input.
        #[test]
        fn matching_never_panics(p in ".{0,20}", s in ".{0,20}") {
            if let Ok(tokens) = compile(&p, &BTreeMap::new(), true) {
                let _ = matches(&tokens, &s, true);
            }
        }
    }
}
