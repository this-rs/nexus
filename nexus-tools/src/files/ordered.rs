//! JSON that remembers the order of its keys.
//!
//! A notebook is rewritten key for key as it was found: reordering the keys of every cell
//! would turn a one-cell edit into a whole-file diff. `serde_json`'s `preserve_order`
//! feature does this, but features are unified across the whole workspace and it changes
//! the key order of every other crate's `Value` (the harness's byte-exact control messages
//! depend on the sorted order). So this small type does it, here only.

use serde_json::Value;

/// A JSON value with ordered objects. Numbers keep their written form.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Self::Object(entries) => entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Sets `key`, keeping its place if it exists, appending it otherwise.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Self::Object(entries) = self {
            match entries.iter_mut().find(|(k, _)| k == key) {
                Some((_, slot)) => *slot = value,
                None => entries.push((key.to_owned(), value)),
            }
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn string(s: impl Into<String>) -> Self {
        Self::String(s.into())
    }

    pub fn empty_object() -> Self {
        Self::Object(Vec::new())
    }

    pub fn empty_array() -> Self {
        Self::Array(Vec::new())
    }

    /// Parses a whole document.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parser = Parser { text, at: 0 };
        let value = parser.value()?;
        parser.skip_space();
        (parser.at == text.len()).then_some(value)
    }

    /// Writes it indented by `indent` (like `serde_json`'s pretty printer: `"k": v`, empty
    /// containers as `[]` and `{}`).
    pub fn pretty(&self, indent: &str) -> String {
        let mut out = String::new();
        self.write(&mut out, indent, 0);
        out
    }

    fn write(&self, out: &mut String, indent: &str, depth: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Number(n) => out.push_str(n),
            Self::String(s) => out.push_str(&quote(s)),
            Self::Array(items) if items.is_empty() => out.push_str("[]"),
            Self::Object(entries) if entries.is_empty() => out.push_str("{}"),
            Self::Array(items) => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&indent.repeat(depth + 1));
                    item.write(out, indent, depth + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                out.push_str(&indent.repeat(depth));
                out.push(']');
            },
            Self::Object(entries) => {
                out.push_str("{\n");
                for (i, (key, value)) in entries.iter().enumerate() {
                    out.push_str(&indent.repeat(depth + 1));
                    out.push_str(&quote(key));
                    out.push_str(": ");
                    value.write(out, indent, depth + 1);
                    out.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
                }
                out.push_str(&indent.repeat(depth));
                out.push('}');
            },
        }
    }
}

fn quote(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        while self.text[self.at..].starts_with([' ', '\n', '\r', '\t']) {
            self.at += 1;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_space();
        if self.text[self.at..].starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_space();
        let rest = &self.text[self.at..];
        match rest.chars().next()? {
            '{' => self.object(),
            '[' => self.array(),
            '"' => self.string().map(Json::String),
            't' if self.eat("true") => Some(Json::Bool(true)),
            'f' if self.eat("false") => Some(Json::Bool(false)),
            'n' if self.eat("null") => Some(Json::Null),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Option<Json> {
        let rest = &self.text[self.at..];
        let len = rest
            .find(|c: char| !(c.is_ascii_digit() || "+-.eE".contains(c)))
            .unwrap_or(rest.len());
        let token = &rest[..len];
        // Let serde_json decide what a number is.
        serde_json::from_str::<serde_json::Number>(token).ok()?;
        self.at += len;
        Some(Json::Number(token.to_owned()))
    }

    fn string(&mut self) -> Option<String> {
        let rest = &self.text[self.at..];
        let mut escaped = false;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => {
                    let token = &rest[..=i];
                    self.at += token.len();
                    return serde_json::from_str::<String>(token).ok();
                },
                _ => escaped = false,
            }
        }
        None
    }

    fn array(&mut self) -> Option<Json> {
        self.eat("[");
        let mut items = Vec::new();
        if self.eat("]") {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            if self.eat("]") {
                return Some(Json::Array(items));
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.eat("{");
        let mut entries = Vec::new();
        if self.eat("}") {
            return Some(Json::Object(entries));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            if !self.eat(":") {
                return None;
            }
            entries.push((key, self.value()?));
            if self.eat("}") {
                return Some(Json::Object(entries));
            }
            if !self.eat(",") {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(text: &str) {
        let parsed = Json::parse(text).unwrap_or_else(|| panic!("did not parse: {text}"));
        assert_eq!(parsed.pretty(" "), text, "round trip");
    }

    #[test]
    fn a_document_in_serde_pretty_form_round_trips_byte_for_byte_in_its_own_key_order() {
        same("{\n \"zebra\": 1,\n \"apple\": [\n  1.0,\n  -2,\n  3e5\n ],\n \"mid\": {}\n}");
        same("[]");
        same("{\n \"s\": \"quote \\\" backslash \\\\ newline \\n tab \\t é 日本 \\u0001\"\n}");
        same(
            "{\n \"nested\": {\n  \"b\": [\n   null,\n   true,\n   false\n  ],\n  \"a\": []\n }\n}",
        );
    }

    #[test]
    fn keys_keep_their_order_and_set_keeps_a_keys_place() {
        let mut doc = Json::parse(r#"{"b":1,"a":2,"c":3}"#).unwrap();
        doc.set("a", Json::Number("9".into()));
        doc.set("z", Json::Null);
        assert_eq!(
            doc.pretty(""),
            "{\n\"b\": 1,\n\"a\": 9,\n\"c\": 3,\n\"z\": null\n}"
        );
    }

    #[test]
    fn what_is_not_json_is_refused() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "{\"a\" 1}",
            "1 2",
            "{\"a\":1} x",
            "\"open",
            "tru",
            "-",
            "[1 2]",
            "{1:2}",
        ] {
            assert!(Json::parse(bad).is_none(), "accepted {bad:?}");
        }
    }
}
