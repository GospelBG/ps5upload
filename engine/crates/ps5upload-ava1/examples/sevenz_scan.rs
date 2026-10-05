//! Read-only header scan: `cargo run --release -p ps5upload-ava1 --example sevenz_scan -- a.7z ...`
//! Opens each archive, reads only its headers (never decodes, never writes) and reports
//! whether any block has stream-less entries inside it (before its last streamed file).
use sevenz_rust2::{Archive, Password};
use std::io::BufReader;

fn main() {
    for p in std::env::args().skip(1) {
        let f = std::fs::File::open(&p).unwrap();
        let mut r = BufReader::new(f);
        let a = match Archive::read(&mut r, &Password::empty()) {
            Ok(a) => a,
            Err(e) => {
                println!("{p}: header error: {e}");
                continue;
            }
        };
        let (mut files, mut empties, mut dirs, mut streamless_in_block) = (0u64, 0u64, 0u64, 0u64);
        for (i, e) in a.files.iter().enumerate() {
            if e.is_directory() {
                dirs += 1;
            } else if e.has_stream() {
                files += 1;
            } else {
                empties += 1;
            }
            if a.stream_map.file_block_index[i].is_some() && !e.has_stream() {
                streamless_in_block += 1;
            }
        }
        println!(
            "{p}: blocks={} streamed_files={files} empty_files={empties} dirs={dirs} streamless_inside_blocks={streamless_in_block}",
            a.blocks.len()
        );
    }
}
