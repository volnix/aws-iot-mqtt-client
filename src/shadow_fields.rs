// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! An editable, typed view of a shadow state section, for the form editor in the Shadows tab.
//!
//! A JSON object becomes a list of [`Field`]s. Strings, numbers, booleans, and nested objects
//! get their own inputs; arrays are edited as inline JSON. [`to_value`] turns the form back into
//! JSON and reports every problem at once, with the field's path. No UI code here.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

/// The type picked for a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Bool,
    Group,
    List,
    Null,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Text,
        Kind::Number,
        Kind::Bool,
        Kind::Group,
        Kind::List,
        Kind::Null,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Text => "Text",
            Kind::Number => "Number",
            Kind::Bool => "True/false",
            Kind::Group => "Group",
            Kind::List => "List",
            Kind::Null => "Null",
        }
    }
}

/// A field's value while it's being edited. Numbers and lists keep the typed text, so partial
/// input such as `1.` isn't lost; they're checked by [`to_value`].
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Text(String),
    Number(String),
    Bool(bool),
    Group(Vec<Field>),
    /// A JSON array, as text.
    List(String),
    Null,
}

impl FieldValue {
    pub fn kind(&self) -> Kind {
        match self {
            FieldValue::Text(_) => Kind::Text,
            FieldValue::Number(_) => Kind::Number,
            FieldValue::Bool(_) => Kind::Bool,
            FieldValue::Group(_) => Kind::Group,
            FieldValue::List(_) => Kind::List,
            FieldValue::Null => Kind::Null,
        }
    }

    /// The value as another type, keeping what carries over (text that's a number, and so on).
    pub fn convert(&self, kind: Kind) -> FieldValue {
        let text = match self {
            FieldValue::Text(s) | FieldValue::Number(s) => s.trim().to_owned(),
            FieldValue::Bool(b) => b.to_string(),
            _ => String::new(),
        };
        match kind {
            Kind::Text => FieldValue::Text(text),
            Kind::Number => FieldValue::Number(if parse_number(&text).is_some() {
                text
            } else {
                "0".into()
            }),
            Kind::Bool => {
                FieldValue::Bool(matches!(self, FieldValue::Bool(true)) || text == "true")
            }
            Kind::Group => match self {
                FieldValue::Group(g) => FieldValue::Group(g.clone()),
                _ => FieldValue::Group(Vec::new()),
            },
            Kind::List => match self {
                FieldValue::List(s) => FieldValue::List(s.clone()),
                _ => FieldValue::List("[]".into()),
            },
            Kind::Null => FieldValue::Null,
        }
    }
}

/// One named value in an object.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Stable across frames, for widget IDs.
    pub id: u64,
    pub key: String,
    pub value: FieldValue,
    /// The field came from the shadow. Its name can't change (an update can't rename), and
    /// removing it sends `null`, which deletes it from the shadow.
    pub existing: bool,
    /// The value before [`Field::remove`], for undo.
    pub removed: Option<Box<FieldValue>>,
}

impl Field {
    pub fn new_empty() -> Self {
        Self {
            id: next_id(),
            key: String::new(),
            value: FieldValue::Text(String::new()),
            existing: false,
            removed: None,
        }
    }

    /// Marks an existing field for deletion. Returns false for a new field, which the caller
    /// should drop instead.
    pub fn remove(&mut self) -> bool {
        if !self.existing {
            return false;
        }
        let old = std::mem::replace(&mut self.value, FieldValue::Null);
        self.removed.get_or_insert(Box::new(old));
        true
    }

    pub fn undo_remove(&mut self) {
        if let Some(old) = self.removed.take() {
            self.value = *old;
        }
    }

    /// Removing an existing field marks it for deletion (`null`); a new field just goes away.
    pub fn is_removal(&self) -> bool {
        self.removed.is_some()
    }
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn parse_number(text: &str) -> Option<serde_json::Number> {
    serde_json::from_str::<serde_json::Number>(text.trim()).ok()
}

/// Whether a number field's text is a valid JSON number.
pub fn is_valid_number(text: &str) -> bool {
    parse_number(text).is_some()
}

/// Whether a list field's text is a JSON array.
pub fn is_valid_list(text: &str) -> bool {
    matches!(serde_json::from_str::<Value>(text), Ok(Value::Array(_)))
}

/// Fields for a JSON object (in key order), all marked as existing. Anything else gives no
/// fields.
pub fn fields_from(value: Option<&Value>) -> Vec<Field> {
    fields_against(value, value)
}

/// Fields for `value`, marking as existing only those also present in `shadow` at the same
/// path. Used when JSON typed by the user is turned into fields.
pub fn fields_against(value: Option<&Value>, shadow: Option<&Value>) -> Vec<Field> {
    let Some(Value::Object(map)) = value else {
        return Vec::new();
    };
    let base = shadow.and_then(Value::as_object);
    map.iter()
        .map(|(key, v)| {
            let in_shadow = base.and_then(|b| b.get(key));
            Field {
                id: next_id(),
                key: key.clone(),
                value: match v {
                    Value::String(s) => FieldValue::Text(s.clone()),
                    Value::Number(n) => FieldValue::Number(n.to_string()),
                    Value::Bool(b) => FieldValue::Bool(*b),
                    Value::Object(_) => FieldValue::Group(fields_against(Some(v), in_shadow)),
                    Value::Array(_) => FieldValue::List(v.to_string()),
                    Value::Null => FieldValue::Null,
                },
                existing: in_shadow.is_some(),
                removed: None,
            }
        })
        .collect()
}

/// The form as a JSON object, or every problem found (each prefixed with the field's path).
pub fn to_value(fields: &[Field]) -> Result<Value, Vec<String>> {
    let mut errors = Vec::new();
    let value = object(fields, "", &mut errors);
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(errors)
    }
}

fn object(fields: &[Field], parent: &str, errors: &mut Vec<String>) -> Value {
    let mut map = Map::new();
    for field in fields {
        let key = field.key.trim();
        let path = if parent.is_empty() {
            key.to_owned()
        } else {
            format!("{parent}.{key}")
        };
        if key.is_empty() {
            let place = if parent.is_empty() {
                String::new()
            } else {
                format!(" in {parent}")
            };
            errors.push(format!("A field{place} needs a name"));
            continue;
        }
        if map.contains_key(key) {
            errors.push(format!("{path}: two fields have this name"));
            continue;
        }
        let value = match &field.value {
            FieldValue::Text(s) => Value::String(s.clone()),
            FieldValue::Number(s) => match parse_number(s) {
                Some(n) => Value::Number(n),
                None => {
                    errors.push(format!("{path}: \"{}\" isn't a number", s.trim()));
                    continue;
                }
            },
            FieldValue::Bool(b) => Value::Bool(*b),
            FieldValue::Group(children) => object(children, &path, errors),
            FieldValue::List(s) => match serde_json::from_str::<Value>(s) {
                Ok(v @ Value::Array(_)) => v,
                _ => {
                    errors.push(format!("{path}: a list looks like [1, \"two\", true]"));
                    continue;
                }
            },
            FieldValue::Null => Value::Null,
        };
        map.insert(key.to_owned(), value);
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_every_json_type() {
        let doc = json!({
            "name": "lamp", "brightness": 72.5, "count": 3, "on": true,
            "color": { "r": 255, "mode": "rgb" }, "tags": ["a", 1], "gone": null,
        });
        let fields = fields_from(Some(&doc));
        assert_eq!(fields.len(), 7);
        assert!(fields.iter().all(|f| f.existing));
        assert_eq!(to_value(&fields).unwrap(), doc);
    }

    #[test]
    fn typed_json_marks_only_shadow_fields_as_existing() {
        let shadow = json!({ "a": 1, "cfg": { "x": 1 } });
        let typed = json!({ "a": 2, "new": true, "cfg": { "x": 2, "y": 3 } });
        let fields = fields_against(Some(&typed), Some(&shadow));
        // Key order depends on serde_json's features, so look fields up by name.
        let existing = |fs: &[Field], key: &str| fs.iter().find(|f| f.key == key).unwrap().existing;
        assert!(existing(&fields, "a"));
        assert!(existing(&fields, "cfg"));
        assert!(!existing(&fields, "new"));
        let cfg = fields.iter().find(|f| f.key == "cfg").unwrap();
        let FieldValue::Group(children) = &cfg.value else {
            panic!()
        };
        assert!(existing(children, "x"));
        assert!(!existing(children, "y"));
    }

    #[test]
    fn non_objects_give_no_fields() {
        assert!(fields_from(None).is_empty());
        assert!(fields_from(Some(&json!([1, 2]))).is_empty());
        assert_eq!(to_value(&[]).unwrap(), json!({}));
    }

    #[test]
    fn reports_every_problem_with_its_path() {
        let mut fields = fields_from(Some(&json!({ "cfg": { "level": 1 } })));
        let FieldValue::Group(children) = &mut fields[0].value else {
            panic!()
        };
        children[0].value = FieldValue::Number("1.2.3".into());
        children.push(Field::new_empty());
        let mut dup = Field::new_empty();
        dup.key = "cfg".into();
        fields.push(dup);
        let mut list = Field::new_empty();
        list.key = "tags".into();
        list.value = FieldValue::List("[1,".into());
        fields.push(list);

        let errors = to_value(&fields).unwrap_err();
        assert_eq!(
            errors,
            [
                "cfg.level: \"1.2.3\" isn't a number",
                "A field in cfg needs a name",
                "cfg: two fields have this name",
                "tags: a list looks like [1, \"two\", true]",
            ]
        );
    }

    #[test]
    fn converting_keeps_what_carries_over() {
        let text = FieldValue::Text(" 42 ".into());
        assert_eq!(text.convert(Kind::Number), FieldValue::Number("42".into()));
        assert_eq!(
            FieldValue::Text("hello".into()).convert(Kind::Number),
            FieldValue::Number("0".into())
        );
        assert_eq!(
            FieldValue::Text("true".into()).convert(Kind::Bool),
            FieldValue::Bool(true)
        );
        assert_eq!(
            FieldValue::Bool(true).convert(Kind::Text),
            FieldValue::Text("true".into())
        );
        assert_eq!(
            FieldValue::Text("x".into()).convert(Kind::List),
            FieldValue::List("[]".into())
        );
        assert_eq!(
            FieldValue::Text("x".into()).convert(Kind::Group),
            FieldValue::Group(Vec::new())
        );
        assert_eq!(
            FieldValue::Number("1".into()).convert(Kind::Null),
            FieldValue::Null
        );
        for kind in Kind::ALL {
            assert_eq!(FieldValue::Null.convert(kind).kind(), kind);
        }
    }

    #[test]
    fn removing_an_existing_field_sends_null_and_can_be_undone() {
        let mut fields = fields_from(Some(&json!({ "a": 1, "b": "x" })));
        assert!(fields[0].remove());
        assert!(fields[0].is_removal());
        assert_eq!(to_value(&fields).unwrap(), json!({ "a": null, "b": "x" }));
        fields[0].undo_remove();
        assert!(!fields[0].is_removal());
        assert_eq!(to_value(&fields).unwrap(), json!({ "a": 1, "b": "x" }));
        let mut new = Field::new_empty();
        assert!(!new.remove(), "a new field is dropped, not sent as null");
    }

    #[test]
    fn validity_checks() {
        assert!(is_valid_number("-1.5e3"));
        assert!(!is_valid_number("1."));
        assert!(!is_valid_number(""));
        assert!(is_valid_list("[]"));
        assert!(!is_valid_list("{}"));
    }
}
