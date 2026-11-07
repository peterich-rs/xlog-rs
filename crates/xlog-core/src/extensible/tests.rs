use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::catalog::{CatalogEntry, FileCatalog, LifecycleState};
use super::crypto::{
    EncryptionDescriptor, EncryptionMetadata, Encryptor, NoEncryption, SealContext,
};
use super::error::ExtensibleError;
use super::format::{write_json_line, FileFormat, JsonLinesFormat, PlainTextFormat};
use super::naming::{FileIdentity, FileNaming, IdFileNaming, RelativeFileName};
use super::rotation::{RotationDecision, RotationPolicy};
use super::store::{ExtensibleStore, StoreConfig};

fn scratch(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "xlog-extensible-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    path
}

fn store_at(root: PathBuf, namespace: &str) -> ExtensibleStore {
    ExtensibleStore::open(
        StoreConfig {
            root,
            namespace: namespace.to_owned(),
            rotation: RotationPolicy::Never,
            retention_secs: 0,
        },
        Box::new(JsonLinesFormat),
        &NoEncryption,
        Box::new(IdFileNaming),
    )
    .unwrap()
    .0
}

#[test]
fn jsonl_write_is_plaintext_and_reopens() {
    let root = scratch("jsonl");
    let mut store = store_at(root.clone(), "app");
    let mut encryptor = NoEncryption;
    store
        .write(br#"{"message":"one"}"#, &mut encryptor)
        .unwrap();
    store
        .write(br#"{"message":"two"}"#, &mut encryptor)
        .unwrap();
    store.close(&mut encryptor).unwrap();

    let files: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".log"))
        .collect();
    assert_eq!(files.len(), 1);
    let text = fs::read_to_string(root.join(&files[0])).unwrap();
    assert_eq!(text, "{\"message\":\"one\"}\n{\"message\":\"two\"}\n");

    let mut reopened = store_at(root.clone(), "app");
    reopened
        .write(br#"{"message":"three"}"#, &mut encryptor)
        .unwrap();
    reopened.close(&mut encryptor).unwrap();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn jsonl_rejects_any_other_encryptor_before_creating_files() {
    let root = scratch("reject");
    let error = ExtensibleStore::open(
        StoreConfig {
            root: root.clone(),
            namespace: "app".to_owned(),
            rotation: RotationPolicy::Never,
            retention_secs: 0,
        },
        Box::new(JsonLinesFormat),
        &RejectingEncryptor,
        Box::new(IdFileNaming),
    );
    assert!(matches!(error, Err(ExtensibleError::Config(_))));
    assert!(!root.exists());
}

#[test]
fn no_encryption_seal_passes_bytes_through() {
    let mut encryptor = NoEncryption;
    let mut output = Vec::new();
    let metadata = encryptor
        .seal(
            &SealContext {
                file_id: 1,
                segment_seq: 0,
                aad: b"segment:0",
            },
            b"{\"a\":1}\n",
            &mut output,
        )
        .unwrap();
    assert_eq!(output, b"{\"a\":1}\n");
    assert_eq!(metadata, EncryptionMetadata::default());
    assert_eq!(encryptor.descriptor().algorithm, "none");
}

#[test]
fn catalog_corruption_refuses_to_open_and_deletes_nothing() {
    let root = scratch("corrupt");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("catalog.v1"), "version 999\nnamespace app\n").unwrap();
    fs::write(root.join("keep.log"), "do not delete").unwrap();

    let error = ExtensibleStore::open(
        StoreConfig {
            root: root.clone(),
            namespace: "app".to_owned(),
            rotation: RotationPolicy::Never,
            retention_secs: 1,
        },
        Box::new(JsonLinesFormat),
        &NoEncryption,
        Box::new(IdFileNaming),
    );
    assert!(matches!(
        error,
        Err(ExtensibleError::RecoveryRequired { .. })
    ));
    assert_eq!(
        fs::read_to_string(root.join("keep.log")).unwrap(),
        "do not delete"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn missing_catalog_in_a_non_empty_directory_is_recovery() {
    let root = scratch("foreign");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("stray.log"), "unowned").unwrap();
    let error = FileCatalog::load(&root, "app");
    assert!(matches!(
        error,
        Err(ExtensibleError::RecoveryRequired { .. })
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn maintenance_keeps_unowned_files_and_expired_sealed_ones_go() {
    let root = scratch("maintain");
    let mut catalog = FileCatalog::empty("app");
    catalog
        .reserve(CatalogEntry {
            file_id: 1,
            name: "old.log".to_owned(),
            format_id: "jsonl".to_owned(),
            format_version: 1,
            sequence: 1,
            created_unix_secs: 1,
            state: LifecycleState::Sealed,
        })
        .unwrap();
    fs::create_dir_all(&root).unwrap();
    catalog.store(&root).unwrap();
    fs::write(root.join("old.log"), "expired").unwrap();
    fs::write(root.join("stray.log"), "keep me").unwrap();

    let mut store = ExtensibleStore::open(
        StoreConfig {
            root: root.clone(),
            namespace: "app".to_owned(),
            rotation: RotationPolicy::Never,
            retention_secs: 1,
        },
        Box::new(JsonLinesFormat),
        &NoEncryption,
        Box::new(IdFileNaming),
    )
    .unwrap()
    .0;
    let report = store.maintain().unwrap();
    assert_eq!(report.deleted, vec!["old.log".to_owned()]);
    assert_eq!(report.unowned, vec!["stray.log".to_owned()]);
    assert!(!root.join("old.log").exists());
    assert!(root.join("stray.log").exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn names_cannot_escape_the_namespace_directory() {
    for bad in ["../secret", "dir/file", "dir\\file", "..", ".", "a\0b", ""] {
        assert!(
            RelativeFileName::new(bad).is_err(),
            "{bad} should be rejected"
        );
    }
    assert_eq!(
        RelativeFileName::new("app.log").unwrap().as_str(),
        "app.log"
    );
}

#[test]
fn id_naming_has_no_date_component() {
    let mut naming = IdFileNaming;
    let name = naming
        .file_name(&FileIdentity {
            namespace: "app".to_owned(),
            file_id: 26,
            sequence: 26,
            created_unix_secs: 1_700_000_000,
            suffix: ".log".to_owned(),
        })
        .unwrap();
    assert_eq!(name.as_str(), "app_000000000000001a.log");
}

#[test]
fn rotation_is_independent_of_the_file_name() {
    let policy = RotationPolicy::Size { max_bytes: 100 };
    assert_eq!(policy.decide(90, 20, 0, 0), RotationDecision::Rotate);
    assert_eq!(policy.decide(0, 500, 0, 0), RotationDecision::Keep);

    let daily = RotationPolicy::Daily;
    assert_eq!(daily.decide(0, 1, 0, 86_400), RotationDecision::Rotate);
    assert_eq!(daily.decide(0, 1, 0, 3_600), RotationDecision::Keep);
}

#[test]
fn json_escaping_handles_quotes_slashes_and_newlines() {
    let mut out = String::new();
    write_json_line(&mut out, "info", "a\"b", "file.rs", "line\nnext\\");
    assert_eq!(
        out,
        "{\"level\":\"info\",\"tag\":\"a\\\"b\",\"target\":\"file.rs\",\"message\":\"line\\nnext\\\\\"}"
    );
}

#[test]
fn plaintext_format_adds_a_missing_newline_only() {
    let format = PlainTextFormat;
    format.validate(&NoEncryption).unwrap();
    let mut encoder = format
        .start_file(&super::format::FileContext {
            file_id: 1,
            namespace: "app".to_owned(),
        })
        .unwrap();
    encoder.push_record(b"already\n").unwrap();
    encoder.push_record(b"needs one").unwrap();
    let mut encryptor = NoEncryption;
    let mut output = Vec::new();
    encoder.seal_segment(&mut encryptor, &mut output).unwrap();
    assert_eq!(output, b"already\nneeds one\n");
}

#[test]
fn journal_replays_a_segment_that_never_reached_the_file() {
    let root = scratch("replay");
    let mut store = store_at(root.clone(), "app");
    let mut encryptor = NoEncryption;
    store.write(b"durable", &mut encryptor).unwrap();
    let name = store.files()[0].name.clone();
    drop(store);

    let journal = super::journal::Journal::open(&root).unwrap();
    journal
        .append(&super::journal::JournalRecord {
            file_id: 1,
            segment_seq: 1,
            offset: fs::metadata(root.join(&name)).unwrap().len(),
            bytes: b"recovered\n".to_vec(),
        })
        .unwrap();

    let (reopened, report) = ExtensibleStore::open(
        StoreConfig {
            root: root.clone(),
            namespace: "app".to_owned(),
            rotation: RotationPolicy::Never,
            retention_secs: 0,
        },
        Box::new(JsonLinesFormat),
        &NoEncryption,
        Box::new(IdFileNaming),
    )
    .unwrap();
    assert_eq!(report.replayed, 1);
    let text = fs::read_to_string(root.join(&name)).unwrap();
    assert!(text.ends_with("recovered\n"), "{text}");
    drop(reopened);
    let _ = fs::remove_dir_all(&root);
}

/// An encryptor no plaintext format is allowed to accept.
struct RejectingEncryptor;

impl Encryptor for RejectingEncryptor {
    fn descriptor(&self) -> EncryptionDescriptor {
        EncryptionDescriptor {
            algorithm: "test",
            version: 1,
        }
    }

    fn seal(
        &mut self,
        _context: &SealContext<'_>,
        _plaintext: &[u8],
        _output: &mut Vec<u8>,
    ) -> Result<EncryptionMetadata, ExtensibleError> {
        Err(ExtensibleError::Encrypt(
            "this encryptor must never be called".to_owned(),
        ))
    }
}
