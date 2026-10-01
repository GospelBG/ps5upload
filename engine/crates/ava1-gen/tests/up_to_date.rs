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
    ];
    for b in bad {
        assert!(ava1_gen::parse(b).is_err(), "accepted: {b}");
    }
}
