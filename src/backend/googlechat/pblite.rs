//! pblite: protobuf written as JSON, which is how Google's BrowserChannel carries Google Chat's events.
//!
//! A message is a JSON array whose index `i` holds field `i + 1`; a trailing JSON object, if there is one, holds the
//! higher field numbers as `{"<number>": value}`. 64-bit integers come as strings, bytes as base64, enums as numbers.
//!
//! Rather than a second set of types for it, the JSON is turned into ordinary protobuf bytes by walking the schema the
//! generated types came from (`proto/googlechat.desc`), and the bytes are read by those types. The other direction -
//! the few requests the forward channel carries - goes the same way backwards.
//!
//! Reading is forgiving on purpose. Google adds fields all the time, and an event the schema does not know all of must
//! still come through: a field it has no number for, or whose value is not the shape the schema says, is left out and
//! the rest of the message kept. Only something that is not an array at all is refused.

use base64::Engine;
use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorSet};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

const DESCRIPTOR: &[u8] = include_bytes!("proto/googlechat.desc");

/// Nesting deeper than this is not followed: `Message.last_reply` is a `Message`, so a hostile frame could nest forever.
const MAX_DEPTH: usize = 64;

struct Schema {
    /// Every message type, by its full name with the leading dot (`.googlechat.Event.EventBody`).
    messages: HashMap<String, DescriptorProto>,
}

fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let set = FileDescriptorSet::decode(DESCRIPTOR).expect("the committed descriptor set decodes");
        let mut messages = HashMap::new();
        for file in &set.file {
            let prefix = format!(".{}", file.package());
            for message in &file.message_type {
                collect(&prefix, message, &mut messages);
            }
        }
        Schema { messages }
    })
}

fn collect(prefix: &str, message: &DescriptorProto, into: &mut HashMap<String, DescriptorProto>) {
    let name = format!("{prefix}.{}", message.name());
    for nested in &message.nested_type {
        collect(&name, nested, into);
    }
    into.insert(name, message.clone());
}

/// The full name a generated type is known by in the schema.
fn full_name(type_name: &str) -> String {
    if type_name.starts_with('.') {
        type_name.to_string()
    } else {
        format!(".googlechat.{type_name}")
    }
}

/// Reads pblite JSON into a generated type. `type_name` is the message's name in the proto (`StreamEventsResponse`).
pub fn decode<M: prost::Message + Default>(json: &Value, type_name: &str) -> anyhow::Result<M> {
    let bytes = to_protobuf(json, type_name)?;
    Ok(M::decode(bytes.as_slice())?)
}

/// pblite JSON to protobuf bytes, by the schema.
pub fn to_protobuf(json: &Value, type_name: &str) -> anyhow::Result<Vec<u8>> {
    let Value::Array(items) = json else {
        anyhow::bail!("pblite: expected an array");
    };
    let schema = schema();
    let descriptor = schema.messages.get(&full_name(type_name)).ok_or_else(|| anyhow::anyhow!("pblite: no message type {type_name}"))?;
    let mut out = Vec::new();
    write_message(schema, descriptor, items, &mut out, 0);
    Ok(out)
}

/// The array's entries as (field number, value): the positional ones, then the trailing object's.
fn entries(items: &[Value]) -> Vec<(u64, &Value)> {
    let (positional, sparse) = match items.last() {
        Some(Value::Object(map)) => (&items[..items.len() - 1], Some(map)),
        _ => (items, None),
    };
    let mut out: Vec<(u64, &Value)> = positional.iter().enumerate().map(|(i, v)| (i as u64 + 1, v)).collect();
    if let Some(map) = sparse {
        let mut extra: Vec<(u64, &Value)> = map.iter().filter_map(|(k, v)| k.parse::<u32>().ok().map(|n| (n as u64, v))).collect();
        extra.sort_by_key(|(n, _)| *n);
        out.extend(extra);
    }
    out
}

fn write_message(schema: &Schema, descriptor: &DescriptorProto, items: &[Value], out: &mut Vec<u8>, depth: usize) {
    if depth >= MAX_DEPTH {
        return;
    }
    for (number, value) in entries(items) {
        if value.is_null() {
            continue;
        }
        let Some(field) = descriptor.field.iter().find(|f| f.number() as u64 == number) else {
            continue;
        };
        if field.label() == Label::Repeated {
            let Value::Array(list) = value else { continue };
            for item in list.iter().filter(|v| !v.is_null()) {
                write_field(schema, field, item, out, depth);
            }
        } else {
            write_field(schema, field, value, out, depth);
        }
    }
}

fn write_field(schema: &Schema, field: &FieldDescriptorProto, value: &Value, out: &mut Vec<u8>, depth: usize) {
    let number = field.number() as u64;
    match field.r#type() {
        Type::Message => {
            let (Value::Array(items), Some(nested)) = (value, schema.messages.get(field.type_name())) else { return };
            let mut inner = Vec::new();
            write_message(schema, nested, items, &mut inner, depth + 1);
            write_len(number, &inner, out);
        }
        Type::String => {
            if let Value::String(s) = value {
                write_len(number, s.as_bytes(), out);
            }
        }
        Type::Bytes => {
            if let Some(bytes) = value.as_str().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()) {
                write_len(number, &bytes, out);
            }
        }
        Type::Bool => {
            let flag = match value {
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
                _ => return,
            };
            write_varint_field(number, flag as u64, out);
        }
        Type::Int32 | Type::Int64 | Type::Uint32 | Type::Uint64 | Type::Enum => {
            if let Some(n) = integer(value) {
                write_varint_field(number, n as u64, out);
            }
        }
        Type::Sint32 | Type::Sint64 => {
            if let Some(n) = integer(value) {
                write_varint_field(number, ((n << 1) ^ (n >> 63)) as u64, out);
            }
        }
        Type::Fixed32 | Type::Sfixed32 => {
            if let Some(n) = integer(value) {
                write_tag(number, 5, out);
                out.extend_from_slice(&(n as u32).to_le_bytes());
            }
        }
        Type::Fixed64 | Type::Sfixed64 => {
            if let Some(n) = integer(value) {
                write_tag(number, 1, out);
                out.extend_from_slice(&(n as u64).to_le_bytes());
            }
        }
        Type::Float => {
            if let Some(f) = float(value) {
                write_tag(number, 5, out);
                out.extend_from_slice(&(f as f32).to_le_bytes());
            }
        }
        Type::Double => {
            if let Some(f) = float(value) {
                write_tag(number, 1, out);
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
        Type::Group => {}
    }
}

/// A number written as a JSON number or, for 64 bits, as a string. Unsigned values past `i64::MAX` keep their bits.
fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)).or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse::<i64>().ok().or_else(|| s.parse::<u64>().ok().map(|u| u as i64)).or_else(|| s.parse::<f64>().ok().map(|f| f as i64)),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn write_tag(number: u64, wire: u64, out: &mut Vec<u8>) {
    write_varint((number << 3) | wire, out);
}

fn write_varint_field(number: u64, value: u64, out: &mut Vec<u8>) {
    write_tag(number, 0, out);
    write_varint(value, out);
}

fn write_len(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
    write_tag(number, 2, out);
    write_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A generated type as pblite JSON, for the forward channel.
pub fn encode<M: prost::Message>(message: &M, type_name: &str) -> anyhow::Result<Value> {
    let schema = schema();
    let descriptor = schema.messages.get(&full_name(type_name)).ok_or_else(|| anyhow::anyhow!("pblite: no message type {type_name}"))?;
    read_message(schema, descriptor, &message.encode_to_vec(), 0)
}

fn read_message(schema: &Schema, descriptor: &DescriptorProto, mut bytes: &[u8], depth: usize) -> anyhow::Result<Value> {
    anyhow::ensure!(depth < MAX_DEPTH, "pblite: nested too deeply");
    let mut fields: Vec<(u64, Value)> = Vec::new();
    while !bytes.is_empty() {
        let key = read_varint(&mut bytes)?;
        let (number, wire) = (key >> 3, key & 7);
        let raw = match wire {
            0 => Raw::Varint(read_varint(&mut bytes)?),
            1 => Raw::Fixed(take(&mut bytes, 8)?),
            2 => {
                let len = read_varint(&mut bytes)? as usize;
                Raw::Len(take(&mut bytes, len)?)
            }
            5 => Raw::Fixed(take(&mut bytes, 4)?),
            _ => anyhow::bail!("pblite: wire type {wire} is not read"),
        };
        let Some(field) = descriptor.field.iter().find(|f| f.number() as u64 == number) else { continue };
        let value = match (field.r#type(), raw) {
            (Type::Message, Raw::Len(b)) => match schema.messages.get(field.type_name()) {
                Some(nested) => read_message(schema, nested, b, depth + 1)?,
                None => continue,
            },
            (Type::String, Raw::Len(b)) => Value::String(String::from_utf8_lossy(b).into_owned()),
            (Type::Bytes, Raw::Len(b)) => Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
            (Type::Bool, Raw::Varint(v)) => Value::Bool(v != 0),
            (Type::Int64 | Type::Sint64 | Type::Sfixed64, Raw::Varint(v)) => Value::String(signed(field.r#type(), v).to_string()),
            (Type::Uint64 | Type::Fixed64, Raw::Varint(v)) => Value::String(v.to_string()),
            (Type::Int32 | Type::Sint32 | Type::Enum, Raw::Varint(v)) => Value::from(signed(field.r#type(), v) as i32),
            (Type::Uint32, Raw::Varint(v)) => Value::from(v as u32),
            (Type::Fixed64 | Type::Sfixed64 | Type::Double, Raw::Fixed(b)) if b.len() == 8 => {
                let n = u64::from_le_bytes(b.try_into()?);
                match field.r#type() {
                    Type::Double => Value::from(f64::from_bits(n)),
                    Type::Sfixed64 => Value::String((n as i64).to_string()),
                    _ => Value::String(n.to_string()),
                }
            }
            (Type::Fixed32 | Type::Sfixed32 | Type::Float, Raw::Fixed(b)) if b.len() == 4 => {
                let n = u32::from_le_bytes(b.try_into()?);
                match field.r#type() {
                    Type::Float => Value::from(f32::from_bits(n) as f64),
                    Type::Sfixed32 => Value::from(n as i32),
                    _ => Value::from(n),
                }
            }
            _ => continue,
        };
        fields.push((number, value));
    }
    let mut array: Vec<Value> = Vec::new();
    for (number, value) in fields {
        let index = number as usize - 1;
        if array.len() <= index {
            array.resize(index + 1, Value::Null);
        }
        let repeated = descriptor.field.iter().any(|f| f.number() as u64 == number && f.label() == Label::Repeated);
        if repeated {
            match &mut array[index] {
                Value::Array(list) => list.push(value),
                slot => *slot = Value::Array(vec![value]),
            }
        } else {
            array[index] = value;
        }
    }
    Ok(Value::Array(array))
}

enum Raw<'a> {
    Varint(u64),
    Fixed(&'a [u8]),
    Len(&'a [u8]),
}

fn signed(kind: Type, v: u64) -> i64 {
    match kind {
        Type::Sint32 | Type::Sint64 => ((v >> 1) as i64) ^ -((v & 1) as i64),
        _ => v as i64,
    }
}

fn read_varint(bytes: &mut &[u8]) -> anyhow::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or_else(|| anyhow::anyhow!("pblite: truncated varint"))?;
        *bytes = rest;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("pblite: varint too long")
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> anyhow::Result<&'a [u8]> {
    anyhow::ensure!(bytes.len() >= len, "pblite: truncated field");
    let (head, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::super::pb;
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_message_event_with_strings_for_int64_and_a_sparse_tail() {
        // Event { group_id: { dm_id: { dm_id: "D1" } }, type: MESSAGE_POSTED, body: { message_posted: { message: {
        //   id: { message_id: "M1" }, creator: { user_id: { id: "42" } }, create_time: 1700000000123456,
        //   text_body: "hi" , reply_to (37): { create_time: 5 } } } } }
        let message = json!([["", "M1"], [["42"]], "1700000000123456", null, null, null, null, null, null, "hi", {"37": [null, "5"], "999": "ignored"}]);
        let event = json!([[null, null, ["D1"]], null, 6, [null, null, null, null, null, [message]]]);
        let response = json!([event]);
        let decoded: pb::StreamEventsResponse = decode(&response, "StreamEventsResponse").unwrap();
        let event = decoded.event.unwrap();
        assert_eq!(event.r#type(), pb::event::EventType::MessagePosted);
        assert_eq!(super::super::Group::from_pb(event.group_id.as_ref()), Some(super::super::Group::Dm("D1".into())));
        let Some(pb::event::event_body::Type::MessagePosted(posted)) = event.body.unwrap().r#type else { panic!("no message") };
        let message = posted.message.unwrap();
        assert_eq!(message.text_body(), "hi");
        assert_eq!(message.create_time(), 1_700_000_000_123_456);
        assert_eq!(message.id.unwrap().message_id(), "M1");
        assert_eq!(message.creator.unwrap().user_id.unwrap().id(), "42");
        assert_eq!(message.reply_to.unwrap().create_time(), 5);
    }

    #[test]
    fn skips_values_of_the_wrong_shape_and_keeps_the_rest() {
        let message = json!([{"bad": 1}, ["x"], "not a number", null, null, null, null, null, null, "kept"]);
        let decoded: pb::Message = decode(&message, "Message").unwrap();
        assert_eq!(decoded.text_body(), "kept");
        assert!(decoded.create_time.is_none());
        assert!(decode::<pb::Message>(&json!("noop"), "Message").is_err());
    }

    #[test]
    fn writes_the_ping_the_way_the_web_client_does() {
        let request = pb::StreamEventsRequest {
            ping_event: Some(pb::PingEvent {
                state: Some(pb::ping_event::State::Active as i32),
                application_focus_state: Some(pb::ping_event::ApplicationFocusState::FocusStateForeground as i32),
                client_interactive_state: Some(pb::ping_event::ClientInteractiveState::Interactive as i32),
                client_notifications_enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = encode(&request, "StreamEventsRequest").unwrap();
        assert_eq!(json, json!([null, [1, null, 1, null, 1, true]]));
        let back: pb::StreamEventsRequest = decode(&json, "StreamEventsRequest").unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn sixty_four_bit_values_are_strings_on_the_way_out() {
        let ping = pb::PingEvent { last_interactive_time_ms: Some(1_700_000_000_000), ..Default::default() };
        assert_eq!(encode(&ping, "PingEvent").unwrap(), json!([null, null, null, "1700000000000"]));
    }
}
