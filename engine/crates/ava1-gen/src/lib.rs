//! Generates the AVA1 codecs from `protocol/ava1/schema/ava1.toml` (SPEC.md §3).
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    #[serde(default, rename = "const")]
    pub consts: Vec<Const>,
    #[serde(default, rename = "message")]
    pub messages: Vec<Msg>,
    #[serde(default, rename = "struct")]
    pub structs: Vec<Msg>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Const {
    pub name: String,
    pub ty: String,
    pub value: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Msg {
    pub name: String,
    #[serde(default, rename = "type")]
    pub frame_type: Option<u8>,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub ext: Vec<Ext>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    pub ty: Ty,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ext {
    pub tag: u16,
    pub name: String,
    pub ty: Ty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ty {
    U8,
    U16,
    U32,
    U64,
    B16,
    B32,
    Bytes,
    Str,
}

fn is_snake(n: &str) -> bool {
    !n.is_empty()
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn parse(src: &str) -> Result<Schema, String> {
    let mut s: Schema = toml::from_str(src).map_err(|e| e.to_string())?;
    for c in &s.consts {
        if !matches!(c.ty.as_str(), "u8" | "u16" | "u32" | "u64") {
            return Err(format!("const {}: bad ty {}", c.name, c.ty));
        }
    }
    let mut names = HashSet::new();
    for m in s.messages.iter().chain(&s.structs) {
        let camel = m
            .name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
            && m.name.chars().all(|c| c.is_ascii_alphanumeric());
        if !camel {
            return Err(format!("{}: message names are CamelCase", m.name));
        }
        if !names.insert(m.name.clone()) {
            return Err(format!("{}: name used twice", m.name));
        }
        let mut fields = HashSet::new();
        for n in m
            .fields
            .iter()
            .map(|f| &f.name)
            .chain(m.ext.iter().map(|e| &e.name))
        {
            if !is_snake(n) {
                return Err(format!("{}.{n}: field names are snake_case", m.name));
            }
            if !fields.insert(n.clone()) {
                return Err(format!("{}.{n}: field used twice", m.name));
            }
        }
        let mut tags = HashSet::new();
        for e in &m.ext {
            if e.tag == 0 || !tags.insert(e.tag) {
                return Err(format!("{}: ext tag {} is 0 or repeated", m.name, e.tag));
            }
        }
    }
    let mut types = HashSet::new();
    for m in &s.messages {
        let t = m
            .frame_type
            .ok_or_else(|| format!("{}: a message needs a type", m.name))?;
        if t >= 0x80 {
            return Err(format!(
                "{}: type {t:#04x} is in the reserved range",
                m.name
            ));
        }
        if !types.insert(t) {
            return Err(format!("{}: type {t:#04x} is used twice", m.name));
        }
    }
    if let Some(st) = s.structs.iter().find(|st| st.frame_type.is_some()) {
        return Err(format!("{}: a struct has no frame type", st.name));
    }
    for m in s.messages.iter_mut().chain(s.structs.iter_mut()) {
        m.ext.sort_by_key(|e| e.tag);
    }
    Ok(s)
}

fn rust_ty(t: Ty) -> &'static str {
    match t {
        Ty::U8 => "u8",
        Ty::U16 => "u16",
        Ty::U32 => "u32",
        Ty::U64 => "u64",
        Ty::B16 => "[u8; 16]",
        Ty::B32 => "[u8; 32]",
        Ty::Bytes => "Vec<u8>",
        Ty::Str => "String",
    }
}

fn rust_put(t: Ty, e: &str) -> String {
    match t {
        Ty::U8 => format!("w.u8({e});"),
        Ty::U16 => format!("w.u16({e});"),
        Ty::U32 => format!("w.u32({e});"),
        Ty::U64 => format!("w.u64({e});"),
        Ty::B16 | Ty::B32 => format!("w.fixed(&{e});"),
        Ty::Bytes => format!("w.bytes(&{e})?;"),
        Ty::Str => format!("w.str(&{e})?;"),
    }
}

fn rust_put_ext(t: Ty) -> &'static str {
    match t {
        Ty::U8 => "w.u8(*v); Ok(())",
        Ty::U16 => "w.u16(*v); Ok(())",
        Ty::U32 => "w.u32(*v); Ok(())",
        Ty::U64 => "w.u64(*v); Ok(())",
        Ty::B16 | Ty::B32 => "w.fixed(v); Ok(())",
        Ty::Bytes => "w.bytes(v)",
        Ty::Str => "w.str(v)",
    }
}

fn rust_get(t: Ty, r: &str) -> String {
    match t {
        Ty::U8 => format!("{r}.u8()?"),
        Ty::U16 => format!("{r}.u16()?"),
        Ty::U32 => format!("{r}.u32()?"),
        Ty::U64 => format!("{r}.u64()?"),
        Ty::B16 => format!("{r}.fixed::<16>()?"),
        Ty::B32 => format!("{r}.fixed::<32>()?"),
        Ty::Bytes => format!("{r}.bytes()?"),
        Ty::Str => format!("{r}.str()?"),
    }
}

fn rust_sample(t: Ty) -> &'static str {
    match t {
        Ty::U8 => "rng.next_u64() as u8",
        Ty::U16 => "rng.next_u64() as u16",
        Ty::U32 => "rng.next_u64() as u32",
        Ty::U64 => "rng.next_u64()",
        Ty::B16 => "{ let mut a = [0u8; 16]; rng.fill(&mut a); a }",
        Ty::B32 => "{ let mut a = [0u8; 32]; rng.fill(&mut a); a }",
        Ty::Bytes => {
            "{ let n = rng.below(41) as usize; let mut v = vec![0u8; n]; rng.fill(&mut v); v }"
        }
        Ty::Str => "rng.ascii(20)",
    }
}

fn emit_rust_msg(o: &mut String, m: &Msg) {
    let _ = writeln!(
        o,
        "#[derive(Debug, Clone, PartialEq, Eq, Default)]\npub struct {} {{",
        m.name
    );
    for f in &m.fields {
        let _ = writeln!(o, "    pub {}: {},", f.name, rust_ty(f.ty));
    }
    for e in &m.ext {
        let _ = writeln!(o, "    pub {}: Option<{}>,", e.name, rust_ty(e.ty));
    }
    o.push_str("}\n\n");
    let _ = writeln!(
        o,
        "impl Message for {} {{\n    const NAME: &'static str = \"{}\";\n",
        m.name, m.name
    );
    o.push_str("    fn encode_into(&self, w: &mut Writer) -> Result<(), EncodeError> {\n");
    for f in &m.fields {
        let _ = writeln!(o, "        {}", rust_put(f.ty, &format!("self.{}", f.name)));
    }
    if m.ext.is_empty() {
        o.push_str("        w.u16(0);\n");
    } else {
        o.push_str("        let mut ext_n: u16 = 0;\n");
        for e in &m.ext {
            let _ = writeln!(o, "        if self.{}.is_some() {{ ext_n += 1; }}", e.name);
        }
        o.push_str("        w.u16(ext_n);\n");
        for e in &m.ext {
            let _ = writeln!(
                o,
                "        if let Some(v) = &self.{} {{ w.ext({}, |w| {{ {} }})?; }}",
                e.name,
                e.tag,
                rust_put_ext(e.ty)
            );
        }
    }
    o.push_str("        Ok(())\n    }\n\n");
    o.push_str("    fn decode(b: &[u8]) -> Result<Self, DecodeError> {\n        let mut r = Reader::new(b);\n");
    let assigns = !m.fields.is_empty() || !m.ext.is_empty();
    let _ = writeln!(
        o,
        "        let {}m = Self::default();",
        if assigns { "mut " } else { "" }
    );
    for f in &m.fields {
        let _ = writeln!(o, "        m.{} = {};", f.name, rust_get(f.ty, "r"));
    }
    o.push_str("        let ext_n = r.u16()?;\n        for _ in 0..ext_n {\n            let tag = r.u16()?;\n            let len = r.u32()? as usize;\n            let v = r.take(len)?;\n");
    if m.ext.is_empty() {
        o.push_str("            let _ = (tag, v);\n");
    } else {
        o.push_str("            match tag {\n");
        for e in &m.ext {
            let _ = writeln!(
                o,
                "                {} => {{\n                    if m.{}.is_some() {{ return Err(DecodeError::DupExt({})); }}\n                    let mut vr = Reader::new(v);\n                    m.{} = Some({});\n                    vr.finish()?;\n                }}",
                e.tag, e.name, e.tag, e.name, rust_get(e.ty, "vr")
            );
        }
        o.push_str("                _ => {}\n            }\n");
    }
    o.push_str("        }\n        r.finish()?;\n        Ok(m)\n    }\n}\n\n");
    if let Some(t) = m.frame_type {
        let _ = writeln!(
            o,
            "impl FrameMessage for {} {{\n    const TYPE: u8 = {t:#04x};\n}}\n",
            m.name
        );
    }
}

pub fn emit_rust(s: &Schema) -> String {
    let mut o = String::new();
    o.push_str("// Generated by engine/crates/ava1-gen from protocol/ava1/schema/ava1.toml. Do not edit:\n// run `cargo run -p ava1-gen` from engine/ after changing the schema.\n");
    o.push_str("#![allow(clippy::field_reassign_with_default, clippy::single_match, clippy::match_single_binding)]\n\n");
    o.push_str("use crate::wire::{DecodeError, EncodeError, FrameMessage, Message, Reader, SplitMix, Writer};\n\n");
    for c in &s.consts {
        let _ = writeln!(o, "pub const {}: {} = {};", c.name, c.ty, c.value);
    }
    o.push('\n');
    let all: Vec<&Msg> = s.messages.iter().chain(&s.structs).collect();
    for m in &all {
        emit_rust_msg(&mut o, m);
    }
    o.push_str(
        "/// Every message and struct, by name (conformance tests).\npub const ALL: &[&str] = &[",
    );
    for m in &all {
        let _ = write!(o, "\"{}\", ", m.name);
    }
    o.push_str("];\n\n#[doc(hidden)]\npub fn sample(name: &str, rng: &mut SplitMix) -> Option<Vec<u8>> {\n    match name {\n");
    for m in &all {
        let _ = write!(o, "        \"{}\" => {} {{", m.name, m.name);
        for f in &m.fields {
            let _ = write!(o, " {}: {},", f.name, rust_sample(f.ty));
        }
        for e in &m.ext {
            let _ = write!(
                o,
                " {}: if rng.below(2) == 1 {{ Some({}) }} else {{ None }},",
                e.name,
                rust_sample(e.ty)
            );
        }
        o.push_str(" }.to_bytes().ok(),\n");
    }
    o.push_str("        _ => None,\n    }\n}\n\n#[doc(hidden)]\npub fn roundtrip(name: &str, bytes: &[u8]) -> Option<Result<Vec<u8>, String>> {\n    fn rt<M: Message>(b: &[u8]) -> Result<Vec<u8>, String> {\n        M::decode(b).map_err(|e| e.to_string())?.to_bytes().map_err(|e| e.to_string())\n    }\n    Some(match name {\n");
    for m in &all {
        let _ = writeln!(o, "        \"{}\" => rt::<{}>(bytes),", m.name, m.name);
    }
    o.push_str("        _ => return None,\n    })\n}\n");
    o
}

/// Every generated file and its content, paths under the repo root.
pub fn outputs(root: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let src = std::fs::read_to_string(root.join("protocol/ava1/schema/ava1.toml"))
        .map_err(|e| format!("read schema: {e}"))?;
    let s = parse(&src)?;
    Ok(vec![(
        root.join("engine/crates/ava1/src/gen.rs"),
        emit_rust(&s),
    )])
}
