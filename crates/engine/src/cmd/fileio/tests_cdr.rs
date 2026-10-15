//! CDR format integration and regressions. Real user art stays local and is never a fixture.

use super::*;
use proptest::prelude::*;
use serde_json::json;

#[test]
fn cdr_is_a_desktop_reader_with_open_and_place_filters_but_no_writer() {
    let f = format("CDR").unwrap();
    assert!(!f.write && !f.raster);
    assert_eq!(f.read, !cfg!(target_arch = "wasm32"));
    assert!(!SAVE_FORMATS.contains(&"cdr"));
    #[cfg(not(target_arch = "wasm32"))]
    {
        assert!(OPEN_EXTS.contains(&"cdr") && PLACE_EXTS.contains(&"cdr"));
        assert!(open_filters().any(|(_, e)| e == ["cdr"]));
        assert!(place_filters().any(|(_, e)| e == ["cdr"]));
    }
    let mut s = Session::new();
    s.execute("file.new", &json!({})).unwrap();
    assert!(save_plan(&s, SaveMode::SaveAs, &json!({"path": "source.cdr"})).is_err());
    assert!(s.execute("document.serialize", &json!({"format": "cdr"})).is_err());
}

#[test]
fn cdr_detection_accepts_riff_and_modern_stream_names_without_treating_all_zip_as_cdr() {
    let riff = b"RIFF\x04\0\0\0CDRG";
    assert!(cdr::sniff(riff));
    #[cfg(not(target_arch = "wasm32"))]
    assert_eq!(detect("unknown.bin", riff).unwrap().id, "cdr");
    for name in ["content/root.dat", "content/riffData.cdr"] {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend(name.as_bytes());
        assert!(cdr::sniff(&zip));
    }
    for bytes in [b"RIFF\x04\0\0\0WEBP".as_slice(), b"PK\x03\x04word/document.xml", b"", b"CDR"] {
        assert!(!cdr::sniff(bytes));
    }
}

#[test]
fn invalid_cdr_does_not_replace_the_active_document() {
    let mut s = Session::new();
    s.execute("file.new", &json!({"width": 123, "height": 45})).unwrap();
    let before = vectorcraft_format::save_file(&s.doc().unwrap().doc);
    for bytes in [b"not a CDR".as_slice(), b"RIFF\x04\0\0\0CDRG", b"RIFF\xff\xff\xff\xffCDRG"] {
        assert!(open_bytes(&mut s, "damaged.cdr", bytes, None).is_err());
    }
    assert_eq!(s.documents().len(), 1);
    assert_eq!(vectorcraft_format::save_file(&s.doc().unwrap().doc), before);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn malformed_cdr_containers_are_bounded_and_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let result = std::panic::catch_unwind(|| load("damaged.cdr", &bytes));
        prop_assert!(result.is_ok());
        prop_assert!(result.unwrap().is_err());
    }
}

/// Run explicitly with VECTORCRAFT_CDR_TEST_FILE set to a user-owned local CDR.
/// Opens through the shared engine; edits, undo, native round-trip and source preservation.
#[test]
#[ignore = "requires a local user-owned CDR and the installed cdr2svg runtime"]
fn local_cdr_opens_as_editable_art_and_saves_without_changing_its_source() {
    let path = std::env::var("VECTORCRAFT_CDR_TEST_FILE").expect("VECTORCRAFT_CDR_TEST_FILE");
    let bytes = std::fs::read(&path).unwrap();
    let mut s = Session::new();
    let r = s.execute("document.open", &json!({"path": path})).unwrap();
    assert_eq!(r["format"], "cdr");
    assert!(!r["warnings"].as_array().unwrap().is_empty());
    let st = s.doc().unwrap();
    assert!(st.path.is_none());
    assert_eq!(st.format, "vectorcraft");
    let mut paths = 0;
    let mut texts = 0;
    st.doc.walk(|n| match n.kind {
        vectorcraft_doc::NodeKind::Path { .. } => paths += 1,
        vectorcraft_doc::NodeKind::Text(_) => texts += 1,
        _ => {}
    });
    assert!(paths > 0 && texts > 0, "editable paths and text, not a preview bitmap");
    let before = vectorcraft_format::save_file(&st.doc);
    s.execute("shape.rectangle", &json!({"x": 0, "y": 0, "width": 10, "height": 10})).unwrap();
    assert_ne!(vectorcraft_format::save_file(&s.doc().unwrap().doc), before);
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(vectorcraft_format::save_file(&s.doc().unwrap().doc), before);
    let native = s.execute("document.serialize", &json!({"format": "vectorcraft"})).unwrap();
    let native = vectorcraft_format::base64_decode(native["dataBase64"].as_str().unwrap()).unwrap();
    let reopened = vectorcraft_format::load_file(&native).unwrap();
    assert_eq!(vectorcraft_format::save_file(&reopened.doc), before);
    let save = save_plan(&s, SaveMode::Save, &json!({})).unwrap();
    assert!(save.path.is_none() && save.name.ends_with(".vectorcraft"));
    assert!(s.execute("document.save", &json!({"path": path})).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let mut placed = Session::new();
    placed.execute("file.new", &json!({"width": 100, "height": 100})).unwrap();
    let clean = vectorcraft_format::save_file(&placed.doc().unwrap().doc);
    let r = placed.execute("file.place", &json!({"path": path, "link": false})).unwrap();
    assert_eq!(r["format"], "cdr");
    assert!(!r["ids"].as_array().unwrap().is_empty());
    let mut paths = 0;
    placed.doc().unwrap().doc.walk(|n| {
        if matches!(n.kind, vectorcraft_doc::NodeKind::Path { .. }) {
            paths += 1;
        }
    });
    assert!(paths > 0, "placing CDR must retain the pasteboard vectors");
    placed.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(vectorcraft_format::save_file(&placed.doc().unwrap().doc), clean);
}
