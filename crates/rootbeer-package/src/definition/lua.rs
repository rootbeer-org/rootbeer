use std::time::{Duration, Instant};

use mlua::Lua;
use mlua::LuaSerdeExt;
use serde::Serialize;
use serde_json::Value;

use super::comments::{Comments, Key};

pub fn read<T: serde::de::DeserializeOwned>(source: &str) -> Result<T, String> {
    let (lua, value) = evaluate(source)?;
    lua.from_value(value).map_err(|e| e.to_string())
}

pub(crate) fn evaluate(source: &str) -> Result<(Lua, mlua::Value), String> {
    let lua = Lua::new();
    lua.set_memory_limit(4 * 1024 * 1024)
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    lua.set_interrupt(move |_| {
        if start.elapsed() > Duration::from_secs(1) {
            return Err(mlua::Error::RuntimeError(
                "package definition exceeded execution limit".into(),
            ));
        }
        Ok(mlua::VmState::Continue)
    });
    let environment = lua.create_table().map_err(|e| e.to_string())?;
    let value = lua
        .load(source)
        .set_environment(environment)
        .eval()
        .map_err(|e| e.to_string())?;
    Ok((lua, value))
}

/// Renders `value`, placing each comment beside the field it documented.
pub(crate) fn write_commented(
    value: &impl Serialize,
    comments: &Comments,
) -> Result<String, String> {
    let value = serde_json::to_value(value).map_err(|e| e.to_string())?;
    let renderer = Renderer { comments };
    let header: String = comments
        .header
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
    Ok(format!(
        "{header}return {}\n",
        renderer.field(&value, 0, 0, &mut Vec::new())
    ))
}

fn quote(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            character if character.is_ascii_control() => {
                output.push_str(&format!("\\{:03}", character as u32))
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

struct Renderer<'a> {
    comments: &'a Comments,
}

impl Renderer<'_> {
    fn field(&self, value: &Value, depth: usize, prefix: usize, path: &mut Vec<Key>) -> String {
        match value {
            Value::Null => "nil".into(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::String(value) => quote(value),
            Value::Array(values) if values.is_empty() => "{}".into(),
            Value::Object(values) if values.is_empty() => "{}".into(),
            Value::Array(values) => {
                if values
                    .iter()
                    .all(|value| !value.is_array() && !value.is_object())
                    && !self.comments.has_children(path)
                {
                    let inline = format!(
                        "{{ {} }}",
                        values
                            .iter()
                            .map(|value| self.field(value, depth + 1, 0, path))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    if depth * 4 + prefix + inline.len() < 100 {
                        return inline;
                    }
                }
                let entries = values.iter().enumerate().map(|(index, value)| {
                    path.push(Key::Index(index));
                    let entry = self.field(value, depth + 1, 0, path);
                    let entry = (path.clone(), entry);
                    path.pop();
                    entry
                });
                let entries: Vec<_> = entries.collect();
                self.table(entries, depth, path)
            }
            Value::Object(values) => {
                // A flattened recipe reads `{}` back as a map, so an empty list has no spelling
                // that survives; every list field defaults, so omitting it means the same thing.
                let entries = values.iter().filter(|(_, value)| !is_empty_list(value));
                let entries: Vec<_> = entries
                    .map(|(key, value)| {
                        let name = field(key);
                        path.push(Key::Field(key.clone()));
                        let rendered = self.field(value, depth + 1, name.len() + 3, path);
                        let entry = (path.clone(), format!("{name} = {rendered}"));
                        path.pop();
                        entry
                    })
                    .collect();
                self.table(entries, depth, path)
            }
        }
    }

    fn table(&self, entries: Vec<(Vec<Key>, String)>, depth: usize, path: &[Key]) -> String {
        let indent = "    ".repeat(depth + 1);
        let mut lines = Vec::new();
        for (key, entry) in entries {
            let comment = self.comments.get(&key);
            for line in comment.iter().flat_map(|comment| &comment.leading) {
                lines.push(format!("{indent}{line}"));
            }
            match comment.and_then(|comment| comment.trailing.as_ref()) {
                Some(trailing) => lines.push(format!("{indent}{entry}, {trailing}")),
                None => lines.push(format!("{indent}{entry},")),
            }
        }
        let mut end = path.to_vec();
        end.push(Key::End);
        for line in self
            .comments
            .get(&end)
            .iter()
            .flat_map(|comment| &comment.leading)
        {
            lines.push(format!("{indent}{line}"));
        }
        format!("{{\n{}\n{}}}", lines.join("\n"), "    ".repeat(depth))
    }
}

fn is_empty_list(value: &Value) -> bool {
    value.as_array().is_some_and(Vec::is_empty)
}

fn field(key: &str) -> String {
    let is_identifier = key
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    let is_reserved = matches!(
        key,
        "and"
            | "break"
            | "do"
            | "else"
            | "elseif"
            | "end"
            | "false"
            | "for"
            | "function"
            | "if"
            | "in"
            | "local"
            | "nil"
            | "not"
            | "or"
            | "repeat"
            | "return"
            | "then"
            | "true"
            | "until"
            | "while"
            | "continue"
            | "type"
            | "export"
    );
    if is_identifier && !is_reserved {
        return key.into();
    }
    format!("[{}]", quote(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_untrusted_text_without_executing_it() {
        let input = std::collections::BTreeMap::from([
            (
                "description",
                "Unicode café\n\0\u{7f}123\"; error('bad') --",
            ),
            ("key\\\"", "\\tag{version}"),
            ("end", "reserved Lua keyword"),
        ]);
        let output: std::collections::BTreeMap<String, String> =
            read(&write_commented(&input, &Comments::default()).unwrap()).unwrap();
        for (key, value) in input {
            assert_eq!(output[key], value);
        }
        assert!(read::<String>("return os.getenv('HOME')").is_err());
    }
}
