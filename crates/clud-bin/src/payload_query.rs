//! gjson-style JSON path segments for `payload_rules`.
//!
//! CLIProxyAPI spells payload-rule paths in gjson syntax, and its own examples
//! use query selectors (`config.example.yaml:322-334`):
//! `tools.#(name=="apply_patch")` and the chained
//! `input.#(type=="additional_tools")#.tools.#(name=="functions")#.tools.#(name=="apply_patch")#`.
//! A dotted path alone cannot express "the array element whose `name` is X", so
//! that whole class of rule was unportable. This module owns the path grammar:
//!
//! * `key` — an object member, or an array index when the current value is an
//!   array (`0`, `12`).
//! * `key.sub` — a dotted walk; a numeric segment indexes an array, and a
//!   missing intermediate object is created by a `default`/`override` rule.
//! * `#(field==value)` — the FIRST element of the current array whose `field`
//!   equals `value`.
//! * `#(field==value)#` — every matching element (gjson's all-matches form).
//!
//! An all-matches selector may sit anywhere in a path: the write or removal is
//! applied under every match (upstream's own chained
//! `input.#(type=="additional_tools")#.tools.#(name=="functions")#` needs that).
//!
//! Deliberate limits, each reported rather than silently ignored: `!=`, bare
//! `#` and gjson's `@modifiers` are not accepted.

use serde_json::{Map, Value};

/// One step of a JSON path.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// An object member name, or an array index when applied to an array.
    Key(String),
    /// A gjson query selector over the current array.
    Query(Query),
}

/// A gjson `#(field==value)` selector.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// The member of each candidate element that is compared.
    pub field: String,
    /// The value the member must equal.
    pub value: Value,
    /// `true` for gjson's trailing-`#` all-matches form.
    pub all: bool,
}

/// Split a path into steps. Dots inside a query (`#(a=="x.y")`) do not split.
pub fn steps(path: &str) -> Result<Vec<Step>, String> {
    if path.is_empty() {
        return Err("empty JSON path".to_string());
    }
    let mut raw: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for character in path.chars() {
        if quoted {
            current.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        match character {
            '"' => {
                quoted = true;
                current.push(character);
            }
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            '.' if depth == 0 => {
                raw.push(std::mem::take(&mut current));
            }
            _ => current.push(character),
        }
    }
    if quoted {
        return Err(format!("`{path}` has an unterminated string"));
    }
    if depth != 0 {
        return Err(format!("`{path}` has an unterminated selector"));
    }
    raw.push(current);
    if raw.iter().any(String::is_empty) {
        return Err(format!("`{path}` has an empty path segment"));
    }
    raw.iter().map(|part| step(part, path)).collect()
}

fn step(part: &str, path: &str) -> Result<Step, String> {
    if !part.starts_with('#') {
        return Ok(Step::Key(part.to_string()));
    }
    if !part.starts_with("#(") {
        return Err(format!(
            "`{path}`: `{part}` is not a selector; write `#(field==\"value\")`"
        ));
    }
    let (body, all) = if let Some(inner) = part.strip_suffix(")#") {
        (inner, true)
    } else if let Some(inner) = part.strip_suffix(')') {
        (inner, false)
    } else {
        return Err(format!("`{path}`: `{part}` is missing its closing `)`"));
    };
    let body = body
        .strip_prefix("#(")
        .ok_or_else(|| format!("`{path}`: `{part}` is missing its opening `#(`"))?;
    let (field, literal) = body.split_once("==").ok_or_else(|| {
        format!("`{path}`: `{part}` must compare one field with `==`, e.g. #(name==\"x\")")
    })?;
    let field = field.trim();
    if field.is_empty() || field.contains(['(', ')', '#', '=', ' ']) {
        return Err(format!("`{path}`: `{part}` has an invalid field name"));
    }
    let literal = literal.trim();
    if literal.is_empty() {
        return Err(format!("`{path}`: `{part}` compares against nothing"));
    }
    let value: Value = serde_json::from_str(literal)
        .map_err(|_| format!("`{path}`: `{part}` value `{literal}` is not a JSON literal"))?;
    Ok(Step::Query(Query {
        field: field.to_string(),
        value,
        all,
    }))
}

/// The first value `steps` resolves to, or `None` when the path reaches
/// nothing. An all-matches selector can resolve to several elements; the
/// caller that needs all of them walks the payload itself (as `set`/`remove`
/// do), while a condition compares one value.
pub fn resolve<'a>(payload: &'a Value, steps: &[Step]) -> Option<&'a Value> {
    let mut candidates: Vec<&Value> = vec![payload];
    for step in steps {
        let mut next: Vec<&Value> = Vec::new();
        for candidate in &candidates {
            descend(candidate, step, &mut next);
        }
        if next.is_empty() {
            return None;
        }
        candidates = next;
    }
    candidates.first().copied()
}

/// True when `path` resolves to a value that is present and not null. A null
/// counts as missing, which is what makes a `default` rule able to fill it.
pub fn present(payload: &Value, steps: &[Step]) -> bool {
    resolve(payload, steps).is_some_and(|value| !value.is_null())
}

fn descend<'a>(current: &'a Value, step: &Step, out: &mut Vec<&'a Value>) {
    match (step, current) {
        (Step::Key(key), Value::Object(map)) => {
            if let Some(next) = map.get(key) {
                out.push(next);
            }
        }
        (Step::Key(key), Value::Array(items)) => {
            if let Some(next) = key.parse::<usize>().ok().and_then(|index| items.get(index)) {
                out.push(next);
            }
        }
        (Step::Query(query), Value::Array(items)) => {
            for item in items {
                if matches(item, query) {
                    out.push(item);
                    if !query.all {
                        break;
                    }
                }
            }
        }
        _ => {}
    }
}

fn matches(item: &Value, query: &Query) -> bool {
    item.as_object()
        .and_then(|map| map.get(&query.field))
        .is_some_and(|found| *found == query.value)
}

/// Write `value` at `steps`, returning how many nodes it changed (0 when a
/// selector matched nothing, which is not an error).
pub fn set(current: &mut Value, steps: &[Step], value: Value) -> Result<usize, String> {
    let Some((step, rest)) = steps.split_first() else {
        return Ok(0);
    };
    match (step, current) {
        (Step::Key(key), Value::Object(map)) if rest.is_empty() => {
            map.insert(key.clone(), value);
            Ok(1)
        }
        (Step::Key(key), Value::Object(map)) => {
            let entry = map
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if entry.is_null() {
                *entry = Value::Object(Map::new());
            }
            set(entry, rest, value)
        }
        (Step::Key(key), Value::Array(items)) if rest.is_empty() => {
            let index = array_index(key, items.len())?;
            items[index] = value;
            Ok(1)
        }
        (Step::Key(key), Value::Array(items)) => {
            let index = array_index(key, items.len())?;
            set(&mut items[index], rest, value)
        }
        (Step::Query(_), Value::Array(_)) if rest.is_empty() => {
            Err("a selector must be followed by the field to set, e.g. \
             #(name==\"x\").type"
                .to_string())
        }
        (Step::Query(query), Value::Array(items)) => {
            let mut changed = 0;
            for item in items.iter_mut() {
                if !matches(item, query) {
                    continue;
                }
                changed += set(item, rest, value.clone())?;
                if !query.all {
                    break;
                }
            }
            Ok(changed)
        }
        (Step::Query(_), _) => Ok(0),
        (Step::Key(key), _) => Err(format!("`{key}` has no containing object")),
    }
}

/// Remove every node `steps` resolves to, returning how many were removed.
pub fn remove(current: &mut Value, steps: &[Step]) -> usize {
    let Some((step, rest)) = steps.split_first() else {
        return 0;
    };
    match (step, current) {
        (Step::Key(key), Value::Object(map)) if rest.is_empty() => {
            usize::from(map.remove(key).is_some())
        }
        (Step::Key(key), Value::Object(map)) => {
            map.get_mut(key).map_or(0, |child| remove(child, rest))
        }
        (Step::Key(key), Value::Array(items)) => key.parse::<usize>().ok().map_or(0, |index| {
            if index >= items.len() {
                return 0;
            }
            if rest.is_empty() {
                items.remove(index);
                1
            } else {
                remove(&mut items[index], rest)
            }
        }),
        (Step::Query(query), Value::Array(items)) => {
            let mut changed = 0;
            // Descending so that removing one element cannot shift the next.
            for index in (0..items.len()).rev() {
                let hit = matches(&items[index], query);
                if !hit {
                    continue;
                }
                changed += if rest.is_empty() {
                    items.remove(index);
                    1
                } else {
                    remove(&mut items[index], rest)
                };
                if !query.all {
                    break;
                }
            }
            changed
        }
        _ => 0,
    }
}

fn array_index(key: &str, len: usize) -> Result<usize, String> {
    let index = key
        .parse::<usize>()
        .map_err(|_| format!("`{key}` is not an array index"))?;
    if index >= len {
        // Growing an array would invent elements nobody asked for.
        return Err(format!("array index {index} is past the end ({len} items)"));
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(path: &str) -> Vec<Step> {
        steps(path).unwrap()
    }

    #[test]
    fn a_dot_inside_a_quoted_selector_does_not_split_the_path() {
        let path = parse(r#"tools.#(name=="a.b").type"#);
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], Step::Key("tools".to_string()));
        assert_eq!(
            path[1],
            Step::Query(Query {
                field: "name".to_string(),
                value: json!("a.b"),
                all: false,
            })
        );
    }

    #[test]
    fn all_matches_and_scalar_literals_parse() {
        let all = parse(r#"input.#(type=="additional_tools")#.tools"#);
        assert!(matches!(&all[1], Step::Query(query) if query.all));
        let number = parse("items.#(index==3)");
        assert!(matches!(&number[1], Step::Query(query) if query.value == json!(3)));
        let boolean = parse("items.#(ok==true)");
        assert!(matches!(&boolean[1], Step::Query(query) if query.value == json!(true)));
    }

    #[test]
    fn unsupported_or_malformed_selectors_are_named_not_ignored() {
        for path in [
            "#",
            "#(name)",
            r#"#(name!="x")"#,
            r#"#(name=="x""#,
            "#(==1)",
            "a..b",
        ] {
            assert!(steps(path).is_err(), "`{path}` should be refused");
        }
    }

    #[test]
    fn present_set_and_remove_reach_the_selected_element() {
        let mut payload = json!({
            "tools": [{"name": "WebSearch"}, {"name": "apply_patch"}],
            "input": [
                {"type": "additional_tools", "tools": [{"name": "functions"}]},
                {"type": "message", "tools": []}
            ]
        });
        let query = parse(r#"tools.#(name=="apply_patch")"#);
        assert!(present(&payload, &query));
        // A selector alone is a location, not a member: there is no key to set.
        assert!(set(&mut payload, &query, json!("gone")).is_err());
        let rename = parse(r#"tools.#(name=="apply_patch").name"#);
        assert_eq!(set(&mut payload, &rename, json!("gone")).unwrap(), 1);
        assert_eq!(payload["tools"][1]["name"], json!("gone"));

        let absent = parse(r#"tools.#(name=="nope").name"#);
        assert!(!present(&payload, &absent));
        assert_eq!(set(&mut payload, &absent, json!("x")).unwrap(), 0);

        let chained = parse(r#"input.#(type=="additional_tools")#.tools.#(name=="functions")#"#);
        assert_eq!(remove(&mut payload, &chained), 1);
        assert_eq!(payload["input"][0]["tools"], json!([]));
        assert_eq!(payload["input"][1]["tools"], json!([]));
    }
}
