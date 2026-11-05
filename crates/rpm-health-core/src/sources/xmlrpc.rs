//! Just enough XML-RPC for talking to Koji, which has no JSON API.

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};

use crate::error::{Error, Result};

/// An XML-RPC value. Recursive types like this work because `Vec` and
/// `HashMap` store their contents on the heap, which keeps `Value` a known size.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Double(f64),
    String(String),
    Nil,
    Array(Vec<Value>),
    Struct(HashMap<String, Value>),
}

fn parse_error(message: impl std::fmt::Display) -> Error {
    Error::Parse {
        what: "Koji XML-RPC response",
        message: message.to_string(),
    }
}

/// Parses a `<methodResponse>` document into its single return value.
pub fn parse_response(xml: &str) -> Result<Value> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_fault = false;

    loop {
        match reader.read_event().map_err(parse_error)? {
            Event::Start(tag) if tag.name().as_ref() == "fault" => in_fault = true,
            Event::Start(tag) if tag.name().as_ref() == "value" => {
                let value = parse_value(&mut reader)?;
                return if in_fault {
                    Err(Error::KojiFault(fault_message(&value)))
                } else {
                    Ok(value)
                };
            }
            Event::Eof => return Err(parse_error("no value in response")),
            _ => {}
        }
    }
}

/// Pulls the message out of a `{faultCode, faultString}` struct.
pub fn fault_message(fault: &Value) -> String {
    fault
        .get("faultString")
        .and_then(Value::as_str)
        .unwrap_or("unknown fault")
        .to_string()
}

/// Reads up to the closing `</value>`. The opening tag is already consumed.
fn parse_value(reader: &mut Reader<&[u8]>) -> Result<Value> {
    let mut value = None;
    loop {
        match reader.read_event().map_err(parse_error)? {
            Event::Start(tag) => value = Some(parse_typed(reader, &tag)?),
            Event::Empty(tag) => match tag.name().as_ref() {
                "nil" => value = Some(Value::Nil),
                "string" => value = Some(Value::String(String::new())),
                other => return Err(unexpected_tag(other)),
            },
            Event::End(_) => return value.ok_or_else(|| parse_error("empty value")),
            Event::Eof => return Err(parse_error("response ended inside a value")),
            _ => {}
        }
    }
}

fn parse_typed(reader: &mut Reader<&[u8]>, tag: &BytesStart) -> Result<Value> {
    match tag.name().as_ref() {
        "array" => parse_array(reader),
        "struct" => parse_struct(reader),
        "string" => Ok(Value::String(read_text(reader, tag)?)),
        "boolean" => Ok(Value::Bool(read_text(reader, tag)? == "1")),
        "int" | "i4" | "i8" => {
            let text = read_text(reader, tag)?;
            text.parse().map(Value::Int).map_err(parse_error)
        }
        "double" => {
            let text = read_text(reader, tag)?;
            text.parse().map(Value::Double).map_err(parse_error)
        }
        "nil" => {
            read_text(reader, tag)?;
            Ok(Value::Nil)
        }
        other => Err(unexpected_tag(other)),
    }
}

fn parse_array(reader: &mut Reader<&[u8]>) -> Result<Value> {
    let mut items = Vec::new();
    loop {
        match reader.read_event().map_err(parse_error)? {
            Event::Start(tag) if tag.name().as_ref() == "value" => {
                items.push(parse_value(reader)?);
            }
            Event::End(tag) if tag.name().as_ref() == "array" => return Ok(Value::Array(items)),
            Event::Eof => return Err(parse_error("response ended inside an array")),
            _ => {} // <data> and </data>
        }
    }
}

fn parse_struct(reader: &mut Reader<&[u8]>) -> Result<Value> {
    let mut fields = HashMap::new();
    let mut name = None;
    loop {
        match reader.read_event().map_err(parse_error)? {
            Event::Start(tag) if tag.name().as_ref() == "name" => {
                name = Some(read_text(reader, &tag)?);
            }
            Event::Start(tag) if tag.name().as_ref() == "value" => {
                let key = name
                    .take()
                    .ok_or_else(|| parse_error("value without name"))?;
                fields.insert(key, parse_value(reader)?);
            }
            Event::End(tag) if tag.name().as_ref() == "struct" => {
                return Ok(Value::Struct(fields));
            }
            Event::Eof => return Err(parse_error("response ended inside a struct")),
            _ => {} // <member> and </member>
        }
    }
}

fn read_text(reader: &mut Reader<&[u8]>, tag: &BytesStart) -> Result<String> {
    let raw = reader.read_text(tag.name()).map_err(parse_error)?;
    let text = unescape(&raw).map_err(parse_error)?;
    Ok(text.into_owned())
}

fn unexpected_tag(tag: &str) -> Error {
    parse_error(format!("unexpected <{tag}>"))
}

impl Value {
    /// `Option<T>` is either `Some(value)` or `None`. Rust uses it where other
    /// languages use null, and the compiler won't let you forget the `None`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Struct(fields) => fields.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(value: &str) -> String {
        format!(
            "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
        )
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(
            parse_response(&wrap("<int>7</int>")).unwrap(),
            Value::Int(7)
        );
        assert_eq!(
            parse_response(&wrap("<boolean>1</boolean>")).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(parse_response(&wrap("<nil/>")).unwrap(), Value::Nil);
    }

    #[test]
    fn unescapes_strings_and_keeps_empty_ones() {
        assert_eq!(
            parse_response(&wrap("<string>gcc &amp; g++</string>")).unwrap(),
            Value::String("gcc & g++".to_string())
        );
        assert_eq!(
            parse_response(&wrap("<string></string>")).unwrap(),
            Value::String(String::new())
        );
    }

    #[test]
    fn parses_nested_arrays_and_structs() {
        let xml = wrap(
            "<array><data><value><struct><member><name>owner</name>\
             <value><string>alice</string></value></member></struct></value></data></array>",
        );
        let value = parse_response(&xml).unwrap();
        let first = &value.as_array().unwrap()[0];
        assert_eq!(first.get("owner").unwrap().as_str(), Some("alice"));
    }

    #[test]
    fn top_level_fault_becomes_an_error() {
        let xml = "<methodResponse><fault><value><struct><member><name>faultString</name>\
                   <value><string>nope</string></value></member></struct></value></fault></methodResponse>";
        assert!(matches!(parse_response(xml), Err(Error::KojiFault(m)) if m == "nope"));
    }

    #[test]
    fn truncated_response_is_an_error_not_a_hang() {
        assert!(parse_response("<methodResponse><params><param><value><array>").is_err());
    }
}
