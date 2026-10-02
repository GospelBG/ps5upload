use std::path::Path;

#[test]
fn generated_files_are_up_to_date() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for (path, want) in ava1_gen::outputs(&root).unwrap() {
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "{} is stale: run `cargo run -p ava1-gen` from engine/",
            path.display()
        );
    }
}

#[test]
fn the_schema_validator_rejects_mistakes() {
    let bad = [
        "[[message]]\nname = \"x\"\ntype = 1\n",    // not CamelCase
        "[[message]]\nname = \"A\"\n",              // no type
        "[[message]]\nname = \"A\"\ntype = 0x80\n", // reserved range
        "[[message]]\nname = \"A\"\ntype = 1\n[[message]]\nname = \"B\"\ntype = 1\n", // dup type
        "[[message]]\nname = \"A\"\ntype = 1\next = [{ tag = 0, name = \"x\", ty = \"u8\" }]\n",
        "[[message]]\nname = \"A\"\ntype = 1\nfields = [{ name = \"Bad\", ty = \"u8\" }]\n",
        "[[message]]\nname = \"A\"\ntype = 1\nfields = [{ name = \"a\", ty = \"f32\" }]\n",
        // Names that would shadow or collide in the generated code.
        "[[message]]\nname = \"Result\"\ntype = 1\n",
        "[[message]]\nname = \"Reader\"\ntype = 1\n",
        "[[message]]\nname = \"A\"\ntype = 1\nfields = [{ name = \"self\", ty = \"u8\" }]\n",
        "[[message]]\nname = \"A\"\ntype = 1\nfields = [{ name = \"body\", ty = \"bytes\" }, { name = \"body_len\", ty = \"u32\" }]\n",
        "[[message]]\nname = \"A\"\ntype = 1\nfields = [{ name = \"has_x\", ty = \"u8\" }]\next = [{ tag = 1, name = \"x\", ty = \"u8\" }]\n",
        "[[const]]\nname = \"ALL\"\nty = \"u8\"\nvalue = 1\n",
        "[[const]]\nname = \"TYPE_PING\"\nty = \"u8\"\nvalue = 1\n",
        "[[const]]\nname = \"X\"\nty = \"u8\"\nvalue = 1\n[[const]]\nname = \"X\"\nty = \"u8\"\nvalue = 2\n",
        "[[const]]\nname = \"X\"\nty = \"u8\"\nvalue = 256\n",
        "[[const]]\nname = \"lower\"\nty = \"u8\"\nvalue = 1\n",
    ];
    for b in bad {
        assert!(ava1_gen::parse(b).is_err(), "accepted: {b}");
    }
}

#[test]
fn keyword_field_names_are_escaped_not_emitted_raw() {
    let schema = ava1_gen::parse(
        "[[const]]\nname = \"BIG\"\nty = \"u64\"\nvalue = 18446744073709551615\n\
         [[message]]\nname = \"Odd\"\ntype = 1\n\
         fields = [{ name = \"type\", ty = \"u8\" }, { name = \"match\", ty = \"str\" }, { name = \"int\", ty = \"u16\" }, { name = \"w\", ty = \"u8\" }, { name = \"m\", ty = \"u8\" }]\n\
         ext = [{ tag = 1, name = \"default\", ty = \"bytes\" }]\n",
    )
    .unwrap();
    let rust = ava1_gen::emit_rust(&schema);
    assert!(rust.contains("pub r#type: u8,"), "{rust}");
    assert!(rust.contains("pub r#match: String,"));
    assert!(rust.contains("m.r#type = r.u8()?;"));
    assert!(rust.contains("self.r#match"));
    assert!(
        rust.contains("pub int: u16,"),
        "a C keyword is fine in Rust"
    );
    assert!(rust.contains("pub default: Option<Vec<u8>>,"));
    // Fields named like the generated code's locals are reached through self./m. only.
    assert!(rust.contains("w.u8(self.w);") && rust.contains("m.m = r.u8()?;"));
    let h = ava1_gen::emit_c_header(&schema);
    assert!(h.contains("uint16_t int_;"), "{h}");
    assert!(h.contains("const uint8_t *default_;") && h.contains("uint32_t default__len;"));
    assert!(h.contains("int has_default;"));
    assert!(h.contains("uint8_t type;"), "a Rust keyword is fine in C");
    assert!(h.contains("#define AVA1_BIG 18446744073709551615ULL"));
    let c = ava1_gen::emit_c_source(&schema);
    assert!(c.contains("m->int_ = ava1_r_u16(&r);"), "{c}");
    assert!(!c.contains("m->int ") && !c.contains("m->default "));
}
