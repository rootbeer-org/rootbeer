//! Comments in a package file, keyed by the field they document, so rendering keeps them.
//!
//! Recipes are data, so this reads only the table constructor after `return`: strings,
//! comments, keys, and nesting. It never evaluates anything.

use std::collections::BTreeMap;

/// A step from a table to one of its fields.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Field(String),
    Index(usize),
    /// Before the table's closing brace, after its last field.
    End,
}

/// Comments on their own lines above a field, and one after it on the same line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Comment {
    pub leading: Vec<String>,
    pub trailing: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Comments {
    /// Above `return`.
    pub header: Vec<String>,
    pub fields: BTreeMap<Vec<Key>, Comment>,
}

impl Comments {
    pub(crate) fn scan(source: &str) -> Result<Self, String> {
        let tokens = tokenize(source)?;
        let mut scanner = Scanner {
            tokens: &tokens,
            position: 0,
            comments: Self::default(),
        };
        scanner.file()?;
        Ok(scanner.comments)
    }

    pub(crate) fn get(&self, path: &[Key]) -> Option<&Comment> {
        self.fields.get(path)
    }

    /// Whether any field of the table at `path` carries a comment.
    pub(crate) fn has_children(&self, path: &[Key]) -> bool {
        self.fields
            .keys()
            .any(|key| key.len() == path.len() + 1 && key.starts_with(path))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// Its text, and whether a newline separates it from the token before it.
    Comment(String, bool),
    String(String),
    Word(String),
    Symbol(char),
}

fn tokenize(source: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut is_new_line = true;
    while index < characters.len() {
        let character = characters[index];
        if character == '\n' {
            is_new_line = true;
            index += 1;
            continue;
        }
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        if character == '-' && characters.get(index + 1) == Some(&'-') {
            let start = index;
            index += 2;
            if let Some(level) = long_bracket(&characters, index) {
                index = close_long_bracket(&characters, index, level)?;
            } else {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            let text: String = characters[start..index].iter().collect();
            tokens.push(Token::Comment(text.trim_end().to_string(), is_new_line));
            is_new_line = false;
            continue;
        }
        is_new_line = false;
        if character == '"' || character == '\'' {
            let start = index;
            index += 1;
            while index < characters.len() && characters[index] != character {
                if characters[index] == '\\' {
                    index += 1;
                }
                if characters.get(index) == Some(&'\n') {
                    return Err("unterminated string in package file".into());
                }
                index += 1;
            }
            if index >= characters.len() {
                return Err("unterminated string in package file".into());
            }
            index += 1;
            tokens.push(Token::String(characters[start..index].iter().collect()));
            continue;
        }
        if character == '[' {
            if let Some(level) = long_bracket(&characters, index) {
                let start = index;
                index = close_long_bracket(&characters, index, level)?;
                tokens.push(Token::String(characters[start..index].iter().collect()));
                continue;
            }
        }
        if character.is_alphanumeric() || character == '_' {
            let start = index;
            while index < characters.len()
                && (characters[index].is_alphanumeric() || matches!(characters[index], '_' | '.'))
            {
                index += 1;
            }
            tokens.push(Token::Word(characters[start..index].iter().collect()));
            continue;
        }
        tokens.push(Token::Symbol(character));
        index += 1;
    }
    Ok(tokens)
}

/// The level of a `[[` or `[==[` opening at `index`, if one starts there.
fn long_bracket(characters: &[char], index: usize) -> Option<usize> {
    if characters.get(index) != Some(&'[') {
        return None;
    }
    let mut level = 0;
    while characters.get(index + 1 + level) == Some(&'=') {
        level += 1;
    }
    (characters.get(index + 1 + level) == Some(&'[')).then_some(level)
}

fn close_long_bracket(characters: &[char], index: usize, level: usize) -> Result<usize, String> {
    let close: Vec<char> = std::iter::once(']')
        .chain(std::iter::repeat_n('=', level))
        .chain(std::iter::once(']'))
        .collect();
    let mut position = index + level + 2;
    while position + close.len() <= characters.len() {
        if characters[position..position + close.len()] == close[..] {
            return Ok(position + close.len());
        }
        position += 1;
    }
    Err("unterminated long bracket in package file".into())
}

struct Scanner<'a> {
    tokens: &'a [Token],
    position: usize,
    comments: Comments,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.position);
        self.position += 1;
        token
    }

    fn file(&mut self) -> Result<(), String> {
        self.comments.header = self.leading();
        if self.next() != Some(&Token::Word("return".into())) {
            return Err("package file must return a table".into());
        }
        self.table(Vec::new())?;
        let rest = self.leading();
        if !rest.is_empty() {
            self.entry(vec![Key::End]).leading.extend(rest);
        }
        Ok(())
    }

    /// Comments on their own lines before the next token.
    fn leading(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(Token::Comment(text, _)) = self.peek() {
            lines.push(text.clone());
            self.position += 1;
        }
        lines
    }

    fn entry(&mut self, path: Vec<Key>) -> &mut Comment {
        self.comments.fields.entry(path).or_default()
    }

    fn table(&mut self, path: Vec<Key>) -> Result<(), String> {
        if self.next() != Some(&Token::Symbol('{')) {
            return Err("package file values must be table constructors".into());
        }
        let mut index = 0;
        loop {
            let leading = self.leading();
            if self.peek() == Some(&Token::Symbol('}')) {
                self.position += 1;
                if !leading.is_empty() {
                    let mut end = path.clone();
                    end.push(Key::End);
                    self.entry(end).leading = leading;
                }
                return Ok(());
            }
            let key = self.key(&mut index)?;
            let mut field = path.clone();
            field.push(key);
            if !leading.is_empty() {
                self.entry(field.clone()).leading = leading;
            }
            if self.peek() == Some(&Token::Symbol('{')) {
                self.table(field.clone())?;
            } else {
                self.value()?;
            }
            if matches!(self.peek(), Some(Token::Symbol(',' | ';'))) {
                self.position += 1;
            }
            if let Some(Token::Comment(text, false)) = self.peek() {
                let text = text.clone();
                self.position += 1;
                self.entry(field).trailing = Some(text);
            }
        }
    }

    fn key(&mut self, index: &mut usize) -> Result<Key, String> {
        if self.peek() == Some(&Token::Symbol('[')) {
            self.position += 1;
            let Some(Token::String(text)) = self.next().cloned() else {
                return Err("package file keys must be strings".into());
            };
            if self.next() != Some(&Token::Symbol(']')) || self.next() != Some(&Token::Symbol('='))
            {
                return Err("malformed bracketed key in package file".into());
            }
            return Ok(Key::Field(unquote(&text)?));
        }
        if let (Some(Token::Word(word)), Some(Token::Symbol('='))) =
            (self.peek(), self.tokens.get(self.position + 1))
        {
            let word = word.clone();
            self.position += 2;
            return Ok(Key::Field(word));
        }
        *index += 1;
        Ok(Key::Index(*index - 1))
    }

    /// Skips a scalar value, which may span several tokens such as `-1` or `"a" .. "b"`.
    fn value(&mut self) -> Result<(), String> {
        let mut depth = 0usize;
        loop {
            match self.peek() {
                None => return Err("unterminated table in package file".into()),
                Some(Token::Symbol('(' | '{' | '[')) => depth += 1,
                Some(Token::Symbol(')' | ']')) => depth = depth.saturating_sub(1),
                Some(Token::Symbol('}')) if depth == 0 => return Ok(()),
                Some(Token::Symbol('}')) => depth -= 1,
                Some(Token::Symbol(',' | ';')) if depth == 0 => return Ok(()),
                Some(Token::Comment(..)) if depth == 0 => return Ok(()),
                _ => {}
            }
            self.position += 1;
        }
    }
}

fn unquote(text: &str) -> Result<String, String> {
    super::lua::read(&format!("return {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_attach_to_the_fields_they_precede_or_follow() {
        let source = r#"-- header
return {
    -- the name
    name = "tool", -- trailing
    build = {
        configure = {
            "--flag", -- not a comment inside the string
            -- why this flag
            "--other",
        },
        -- closing note
    },
    ["key with spaces"] = "--[[ still a string ]]",
    --[[ a block
    comment ]]
    last = [[long -- string]],
}
"#;
        let comments = Comments::scan(source).unwrap();
        assert_eq!(comments.header, ["-- header"]);
        let field = |keys: &[Key]| comments.get(keys).cloned().unwrap_or_default();
        let name = field(&[Key::Field("name".into())]);
        assert_eq!(name.leading, ["-- the name"]);
        assert_eq!(name.trailing.as_deref(), Some("-- trailing"));
        let configure = vec![Key::Field("build".into()), Key::Field("configure".into())];
        let mut first = configure.clone();
        first.push(Key::Index(0));
        assert_eq!(
            field(&first).trailing.as_deref(),
            Some("-- not a comment inside the string")
        );
        let mut second = configure;
        second.push(Key::Index(1));
        assert_eq!(field(&second).leading, ["-- why this flag"]);
        assert_eq!(
            field(&[Key::Field("build".into()), Key::End]).leading,
            ["-- closing note"]
        );
        assert_eq!(
            field(&[Key::Field("last".into())]).leading,
            ["--[[ a block\n    comment ]]"]
        );
        assert!(comments
            .get(&[Key::Field("key with spaces".into())])
            .is_none());
    }
}
