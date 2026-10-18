//! A small, bounded JSON reader for the style records CorelDRAW embeds (text story styles and the
//! per-object style cache). Objects keep their key order; duplicate keys keep the first value.
//! Nesting, string length and value count are capped, and malformed input is an error, never a
//! panic.

/// Longest JSON text accepted (the sample's largest is under 4 KiB).
pub(crate) const MAX_JSON: usize = 1 << 20;
const MAX_DEPTH: usize = 32;
const MAX_VALUES: usize = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The member `key` of an object.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Follow a chain of object keys.
    pub(crate) fn path(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// A number, or a string holding one (CorelDRAW writes most numbers as strings).
    pub(crate) fn as_f64(&self) -> Option<f64> {
        let v = match self {
            Value::Number(n) => *n,
            Value::Str(s) => s.trim().parse::<f64>().ok()?,
            _ => return None,
        };
        v.is_finite().then_some(v)
    }
}

/// Parse one JSON text (surrounding whitespace allowed).
pub(crate) fn parse(bytes: &[u8]) -> Result<Value, &'static str> {
    if bytes.len() > MAX_JSON {
        return Err("a style record that is too long");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "a style record that is not UTF-8")?;
    let mut p = Parser { b: text.as_bytes(), at: 0, values: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.at != p.b.len() {
        return Err("a style record with trailing data");
    }
    Ok(v)
}

const BAD: &str = "a damaged style record";

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
    values: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), &'static str> {
        if self.peek() != Some(c) {
            return Err(BAD);
        }
        self.at += 1;
        Ok(())
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value, &'static str> {
        if self.b.get(self.at..self.at + word.len()) != Some(word) {
            return Err(BAD);
        }
        self.at += word.len();
        Ok(v)
    }

    fn value(&mut self, depth: usize) -> Result<Value, &'static str> {
        self.values += 1;
        if self.values > MAX_VALUES {
            return Err("a style record with too many values");
        }
        if depth > MAX_DEPTH {
            return Err("a style record nested too deeply");
        }
        match self.peek().ok_or(BAD)? {
            b'{' => {
                self.at += 1;
                let mut members: Vec<(String, Value)> = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Value::Object(members));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    self.eat(b':')?;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    if !members.iter().any(|(k, _)| *k == key) {
                        members.push((key, v));
                    }
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Value::Object(members));
                        }
                        _ => return Err(BAD),
                    }
                }
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::Array(items));
                        }
                        _ => return Err(BAD),
                    }
                }
            }
            b'"' => Ok(Value::Str(self.string()?)),
            b't' => self.literal(b"true", Value::Bool(true)),
            b'f' => self.literal(b"false", Value::Bool(false)),
            b'n' => self.literal(b"null", Value::Null),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Result<Value, &'static str> {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')) {
            self.at += 1;
        }
        let s = std::str::from_utf8(self.b.get(start..self.at).ok_or(BAD)?).map_err(|_| BAD)?;
        let n: f64 = s.parse().map_err(|_| BAD)?;
        if !n.is_finite() {
            return Err(BAD);
        }
        Ok(Value::Number(n))
    }

    fn hex4(&mut self) -> Result<u32, &'static str> {
        let s = self.b.get(self.at..self.at + 4).ok_or(BAD)?;
        let s = std::str::from_utf8(s).map_err(|_| BAD)?;
        let v = u32::from_str_radix(s, 16).map_err(|_| BAD)?;
        self.at += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            // Copy the run of plain characters at once (the input is valid UTF-8, and quotes and
            // backslashes are ASCII, so these offsets fall on character boundaries). Raw control
            // characters are tolerated.
            let start = self.at;
            while matches!(self.peek(), Some(c) if c != b'"' && c != b'\\') {
                self.at += 1;
            }
            out.push_str(std::str::from_utf8(self.b.get(start..self.at).ok_or(BAD)?).map_err(|_| BAD)?);
            match self.peek().ok_or(BAD)? {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let e = self.peek().ok_or(BAD)?;
                    self.at += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) && self.b.get(self.at..self.at + 2) == Some(b"\\u".as_slice()) {
                                self.at += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) { 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00) } else { 0xFFFD }
                            } else {
                                hi
                            };
                            out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
                        }
                        _ => return Err(BAD),
                    }
                }
                _ => return Err(BAD),
            }
        }
    }
}
