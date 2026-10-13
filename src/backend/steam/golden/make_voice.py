#!/usr/bin/env python3
"""Writes voicewire.rs and voice_golden.txt from the Steam client's own JavaScript.

Steam's voice and WebRTC calls are not in the .proto files extracted from
steamclient.so: they are only in the web client, as generated message classes
with their field numbers and types. This reads those tables out of the
prettified UI bundle and writes, from the one table:

  - ../voicewire.rs: a prost struct for each message
  - voice_golden.txt: each message encoded by the reference protobuf library from
    a descriptor built out of the same table, which the tests in voicewire.rs
    require the structs to reproduce byte for byte

    python3 make_voice.py /path/to/steam/decomp/steamui/chunk~2dcc5aaf7.js

The bundle is the source of truth; nothing here is typed in by hand.
"""
import os
import re
import sys

from google.protobuf import descriptor_pb2, descriptor_pool, message_factory

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = open(sys.argv[1]).read()

WANT = re.compile(r"^C(VoiceChat|WebRTCClient|WebRTC)_|^CChatRoom_(JoinVoiceChat|LeaveVoiceChat|NotifyShouldRejoinChatRoomVoiceChat)")
KINDS = {
    "readFixed64String": ("fixed64", "u64", descriptor_pb2.FieldDescriptorProto.TYPE_FIXED64),
    "readUint64String": ("uint64", "u64", descriptor_pb2.FieldDescriptorProto.TYPE_UINT64),
    "readUint32": ("uint32", "u32", descriptor_pb2.FieldDescriptorProto.TYPE_UINT32),
    "readInt32": ("int32", "i32", descriptor_pb2.FieldDescriptorProto.TYPE_INT32),
    "readBool": ("bool", "bool", descriptor_pb2.FieldDescriptorProto.TYPE_BOOL),
    "readString": ("string", "String", descriptor_pb2.FieldDescriptorProto.TYPE_STRING),
    "readBytes": ('bytes = "vec"', "Vec<u8>", descriptor_pb2.FieldDescriptorProto.TYPE_BYTES),
    "readEnum": ("int32", "i32", descriptor_pb2.FieldDescriptorProto.TYPE_INT32),
    "readFloat": ("float", "f32", descriptor_pb2.FieldDescriptorProto.TYPE_FLOAT),
}

# JS class variable -> wire name, for nested message fields.
# Class variables are reused from one module of the bundle to the next, so a name
# is resolved to the nearest definition before the place it is used.
defs = []  # (position, var, wire name, body)
for m in re.finditer(r"class (\w+) extends d\.Message \{(.*?)getClassName\(\) \{\s*return \"(\w+)\";", SRC, re.S):
    var, body, name = m.groups()
    defs.append((m.start(), var, name, body))

def rust_name(wire):
    n = wire[1:] if wire.startswith("C") else wire
    n = n.replace("CSSRCToAccountIDMapping", "SsrcAccount")
    parts = n.split("_")
    if parts[0] in ("VoiceChat", "WebRTCClient", "WebRTC"):
        parts = parts[1:]
    elif parts[0] == "ChatRoom":
        # Both services have a LeaveVoiceChat; the room's is told apart.
        parts = ["Room"] + parts[1:]
    return "".join(p[0].upper() + p[1:] for p in parts)

def resolve(var, before):
    same = [d for d in defs if d[1] == var]
    earlier = [d for d in same if d[0] < before]
    # A message can refer to one defined just after it in the same module.
    return (earlier[-1] if earlier else [d for d in same if d[0] > before][0])[2]

messages = {}  # wire name -> (rust name, [(field, number, kind, rust type, repeated, submsg)])
order = []
for pos, var, name, body in defs:
    if not WANT.search(name):
        continue
    fields = []
    for fm in re.finditer(r"(\w+): \{ n: (\d+), (?:br: m\.qM\.(\w+)|c: (\w+), r: !0)", body):
        fname, num, read, sub = fm.groups()
        if read:
            fields.append((fname, int(num), read, None))
        else:
            fields.append((fname, int(num), None, resolve(sub, pos)))
    messages[name] = (rust_name(name), fields)
    order.append(name)

def sub_wire(wire):
    return wire

# --- Rust ---
rs = ['//! Steam voice: the messages of the VoiceChat and WebRTCClient services and the voice\n',
      '//! calls of ChatRoom, written out by `golden/make_voice.py` from the Steam client\'s own\n',
      '//! JavaScript. Do not edit by hand: change the generator and run it again.\n\n',
      '#![allow(dead_code)]\n\nuse prost::Message;\n\n']
for name in order:
    rname, fields = messages[name]
    rs.append("#[derive(Clone, PartialEq, Message)]\n")
    rs.append(f"pub struct {rname} {{\n")
    for fname, num, read, sub in fields:
        if read:
            attr, ty, _ = KINDS[read]
            rs.append(f'    #[prost({attr}, optional, tag = "{num}")]\n    pub {fname}: Option<{ty}>,\n')
        else:
            inner = messages[sub_wire(sub)][0]
            rs.append(f'    #[prost(message, repeated, tag = "{num}")]\n    pub {fname}: Vec<{inner}>,\n')
    rs.append("}\n\n")
names = ", ".join(messages[n][0] for n in order)
rs.append("#[cfg(test)]\nmod tests {\n    use super::*;\n\n    const GOLDEN: &str = include_str!(\"golden/voice_golden.txt\");\n\n")
rs.append("    fn unhex(s: &str) -> Vec<u8> {\n        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()\n    }\n\n")
rs.append("    fn round_trips<M: Message + Default>(name: &str, bytes: &[u8]) {\n        let decoded = M::decode(bytes).unwrap_or_else(|e| panic!(\"{name}: {e}\"));\n        assert_eq!(decoded.encode_to_vec(), bytes, \"{name} does not encode to what the reference library wrote\");\n    }\n\n")
rs.append("    #[test]\n    fn every_voice_message_matches_the_reference_encoding() {\n        let mut seen = 0;\n        for line in GOLDEN.lines().filter(|l| !l.trim().is_empty()) {\n            let (name, hex) = line.split_once(' ').expect(\"name and bytes\");\n            let bytes = unhex(hex);\n            macro_rules! cases {\n                ($($t:ident),* $(,)?) => {\n                    match name {\n                        $( stringify!($t) => round_trips::<$t>(name, &bytes), )*\n                        other => panic!(\"voice_golden.txt has {other}, which voicewire.rs does not\"),\n                    }\n                };\n            }\n")
rs.append(f"            cases!({names});\n            seen += 1;\n        }}\n        assert_eq!(seen, {len(order)});\n    }}\n}}\n")
open(os.path.join(HERE, "..", "voicewire.rs"), "w").write("".join(rs))

# --- reference encoding ---
fdp = descriptor_pb2.FileDescriptorProto(name="voice.proto", package="v", syntax="proto2")
for name in order:
    rname, fields = messages[name]
    msg = fdp.message_type.add(name=rname)
    for fname, num, read, sub in fields:
        f = msg.field.add(name=fname, number=num, label=descriptor_pb2.FieldDescriptorProto.LABEL_OPTIONAL)
        if read:
            f.type = KINDS[read][2]
        else:
            f.label = descriptor_pb2.FieldDescriptorProto.LABEL_REPEATED
            f.type = descriptor_pb2.FieldDescriptorProto.TYPE_MESSAGE
            f.type_name = ".v." + messages[sub_wire(sub)][0]
pool = descriptor_pool.DescriptorPool()
pool.Add(fdp)

def sample(desc, depth=0):
    values = {}
    for f in desc.fields:
        T = f.type
        if T == f.TYPE_MESSAGE:
            if depth < 2:
                values[f.name] = [sample(f.message_type, depth + 1)]
        elif T == f.TYPE_BOOL:
            values[f.name] = True
        elif T == f.TYPE_STRING:
            values[f.name] = f"s{f.number}"
        elif T == f.TYPE_BYTES:
            values[f.name] = b"\x01\x02\x03"
        elif T == f.TYPE_FLOAT:
            values[f.name] = 1.5
        elif T in (f.TYPE_FIXED64, f.TYPE_UINT64):
            values[f.name] = 2**33 + f.number
        else:
            values[f.name] = 7 + f.number
    return values

def fill(msg, values):
    for k, v in values.items():
        f = msg.DESCRIPTOR.fields_by_name[k]
        if f.type == f.TYPE_MESSAGE:
            for item in v:
                fill(getattr(msg, k).add(), item)
        else:
            setattr(msg, k, v)
    return msg

lines = []
for name in order:
    rname = messages[name][0]
    cls = message_factory.GetMessageClass(pool.FindMessageTypeByName("v." + rname))
    lines.append(f"{rname} {fill(cls(), sample(cls.DESCRIPTOR)).SerializeToString().hex()}")
open(os.path.join(HERE, "voice_golden.txt"), "w").write("\n".join(lines) + "\n")
print(len(order), "messages")
