//! A small JSON reader, for the answers of wirbel itself (`windows`) and of browsers. It reads
//! what RFC 8259 allows and keeps objects in the order they were written.

use std::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut reader = Reader {
            chars: text.char_indices().peekable(),
            text,
            depth: 0,
        };
        let value = reader.value()?;
        reader.space();
        match reader.chars.peek() {
            None => Ok(value),
            Some((at, _)) => Err(format!("unexpected text at byte {at}")),
        }
    }

    /// The member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The member at the end of a path of keys.
    pub fn at(&self, path: &[&str]) -> Option<&Value> {
        path.iter().try_fold(self, |value, key| value.get(key))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> &[Value] {
        match self {
            Self::Array(items) => items,
            _ => &[],
        }
    }

    /// As JSON text again.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
                let _ = write!(out, "{}", *number as i64);
            }
            Self::Number(number) => {
                let _ = write!(out, "{number}");
            }
            Self::String(text) => out.push_str(&quote(text)),
            Self::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(members) => {
                out.push('{');
                for (index, (key, value)) in members.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&quote(key));
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// `text` as a JSON string.
pub fn quote(text: &str) -> String {
    crate::ipc::json_string(text)
}

/// Deeper than this is no answer we expect, and would overflow the stack.
const MAX_DEPTH: usize = 128;

struct Reader<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    text: &'a str,
    depth: usize,
}

impl Reader<'_> {
    fn space(&mut self) {
        while self
            .chars
            .next_if(|(_, c)| " \t\r\n".contains(*c))
            .is_some()
        {}
    }

    fn fail<T>(&mut self, what: &str) -> Result<T, String> {
        match self.chars.peek() {
            Some((at, c)) => Err(format!("expected {what} at byte {at}, found `{c}`")),
            None => Err(format!("expected {what}, found the end")),
        }
    }

    fn expect(&mut self, wanted: char) -> Result<(), String> {
        match self.chars.next_if(|(_, c)| *c == wanted) {
            Some(_) => Ok(()),
            None => self.fail(&format!("`{wanted}`")),
        }
    }

    fn word(&mut self, word: &str, value: Value) -> Result<Value, String> {
        for wanted in word.chars() {
            self.expect(wanted)?;
        }
        Ok(value)
    }

    fn value(&mut self) -> Result<Value, String> {
        self.space();
        match self.chars.peek().map(|(_, c)| *c) {
            Some('n') => self.word("null", Value::Null),
            Some('t') => self.word("true", Value::Bool(true)),
            Some('f') => self.word("false", Value::Bool(false)),
            Some('"') => self.string().map(Value::String),
            Some('[') => self.nested(Self::array),
            Some('{') => self.nested(Self::object),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => self.fail("a value"),
        }
    }

    fn nested(&mut self, read: fn(&mut Self) -> Result<Value, String>) -> Result<Value, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        let value = read(self);
        self.depth -= 1;
        value
    }

    fn array(&mut self) -> Result<Value, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.space();
        if self.chars.next_if(|(_, c)| *c == ']').is_some() {
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.space();
            if self.chars.next_if(|(_, c)| *c == ',').is_none() {
                self.expect(']')?;
                return Ok(Value::Array(items));
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.expect('{')?;
        let mut members = Vec::new();
        self.space();
        if self.chars.next_if(|(_, c)| *c == '}').is_some() {
            return Ok(Value::Object(members));
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            self.expect(':')?;
            members.push((key, self.value()?));
            self.space();
            if self.chars.next_if(|(_, c)| *c == ',').is_none() {
                self.expect('}')?;
                return Ok(Value::Object(members));
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.chars.peek().map_or(self.text.len(), |(at, _)| *at);
        let part = |c: char| c.is_ascii_digit() || "+-.eE".contains(c);
        while self.chars.next_if(|(_, c)| part(*c)).is_some() {}
        let end = self.chars.peek().map_or(self.text.len(), |(at, _)| *at);
        let text = &self.text[start..end];
        let number: f64 = text.parse().map_err(|_| format!("`{text}` is no number"))?;
        Ok(Value::Number(number))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut code = 0;
        for _ in 0..4 {
            let digit = self.chars.next().and_then(|(_, c)| c.to_digit(16));
            code = code * 16 + digit.ok_or("expected four hex digits after \\u")?;
        }
        Ok(code)
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut text = String::new();
        loop {
            match self.chars.next() {
                None => return Err("a string does not end".into()),
                Some((_, '"')) => return Ok(text),
                Some((_, '\\')) => match self.chars.next().map(|(_, c)| c) {
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some('r') => text.push('\r'),
                    Some('b') => text.push('\u{8}'),
                    Some('f') => text.push('\u{c}'),
                    Some(c @ ('"' | '\\' | '/')) => text.push(c),
                    Some('u') => {
                        let mut code = self.hex4()?;
                        // Two escapes make one character outside the first plane.
                        if (0xd800..0xdc00).contains(&code)
                            && self.chars.next_if(|(_, c)| *c == '\\').is_some()
                        {
                            self.expect('u')?;
                            let low = self.hex4()?;
                            code = 0x10000 + ((code - 0xd800) << 10) + (low.wrapping_sub(0xdc00));
                        }
                        text.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    _ => return Err("unknown escape in a string".into()),
                },
                Some((_, c)) => text.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_wirbel_writes() {
        let text = r#"[{"id":1,"app_id":"kitty","title":"a \"b\" \\ c","focused":true,"x":-3.5}]"#;
        let value = Value::parse(text).unwrap();
        let window = &value.as_array()[0];
        assert_eq!(window.get("id").and_then(Value::as_f64), Some(1.0));
        assert_eq!(
            window.get("title").and_then(Value::as_str),
            Some("a \"b\" \\ c")
        );
        assert_eq!(window.get("focused").and_then(Value::as_bool), Some(true));
        assert_eq!(window.get("x").and_then(Value::as_f64), Some(-3.5));
        assert_eq!(window.get("nothing"), None);
        assert_eq!(value.to_json(), text);
    }

    #[test]
    fn reads_the_rest_of_json() {
        let value = Value::parse(" { \"a\" : [ null , false , 1e3 ] , \"b\" : { } , \"c\":[] } ");
        let value = value.unwrap();
        assert_eq!(value.at(&["a"]).unwrap().as_array().len(), 3);
        assert_eq!(
            value.at(&["a"]).unwrap().as_array()[2],
            Value::Number(1000.0)
        );
        assert_eq!(value.at(&["b"]), Some(&Value::Object(Vec::new())));
        assert_eq!(value.at(&["b", "x"]), None);
        assert_eq!(
            Value::parse(r#""\u00e4\ud83d\ude00\n""#),
            Ok(Value::String("ä😀\n".into()))
        );
        assert_eq!(Value::parse("12").unwrap().to_json(), "12");
        assert_eq!(Value::parse("0.5").unwrap().to_json(), "0.5");
    }

    #[test]
    fn refuses_what_is_no_json() {
        for text in [
            "", "[1,", "{\"a\"}", "tru", "[1] x", "\"open", "{a:1}", "01x", "\"\\q\"",
        ] {
            assert!(Value::parse(text).is_err(), "{text}");
        }
        let deep = "[".repeat(1000);
        assert!(Value::parse(&deep).is_err());
    }
}
