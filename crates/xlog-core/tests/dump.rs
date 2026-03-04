use std::fs;

use mars_xlog_core::dump::{dump_to_file, memory_dump};

#[test]
fn memory_dump_outputs_hex_and_ascii() {
    let out = memory_dump(b"hello");
    assert!(out.contains("5 bytes"));
    assert!(out.contains("68 65 6c 6c 6f"));
}

#[test]
fn dump_to_file_writes_dump_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let out = dump_to_file(dir.path().to_str().unwrap(), b"payload");
    assert!(out.contains("dump file to"));

    let mut found = false;
    for entry in fs::read_dir(dir.path()).unwrap().flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        for dump in fs::read_dir(&p).unwrap().flatten() {
            if dump.path().extension().and_then(|x| x.to_str()) == Some("dump") {
                found = true;
            }
        }
    }
    assert!(found, "expected at least one .dump output");
}
