//! PowerClip keeps the complete artwork, its painted frame and editing options. These tests
//! exercise the command boundary, renderer, hit testing, history and native persistence together.

use serde_json::json;
use vectorcraft_doc::hit::{HitOptions, hit_test};
use vectorcraft_doc::{Document, NodeId, NodeKind};
use vectorcraft_geom::{Affine, Point, Rect};
use vectorcraft_render::{RenderOptions, Renderer};

use crate::Session;

fn session() -> Session {
    let mut s = Session::new();
    s.execute("file.new", &json!({"width":200,"height":200})).unwrap();
    s
}
fn rect(s: &mut Session, x: f64, y: f64, w: f64, h: f64, color: &str) -> NodeId {
    let id = NodeId(s.execute("shape.rectangle", &json!({"x":x,"y":y,"width":w,"height":h})).unwrap()["id"].as_u64().unwrap());
    s.execute("paint.setFill", &json!({"ids":[id.0],"color":color})).unwrap();
    id
}
fn setup() -> (Session, NodeId, NodeId, NodeId) {
    let mut s = session();
    let art = rect(&mut s, 40.0, 50.0, 60.0, 100.0, "#ff0000");
    let frame = rect(&mut s, 50.0, 50.0, 100.0, 100.0, "#0000ff");
    s.execute("paint.setStroke", &json!({"ids":[frame.0],"color":"#00ff00"})).unwrap();
    s.execute("stroke.set", &json!({"ids":[frame.0],"weight":10})).unwrap();
    let id = NodeId(s.execute("object.powerClip.make", &json!({"frame":frame.0,"ids":[art.0]})).unwrap()["id"].as_u64().unwrap());
    (s, id, frame, art)
}
fn rgb(doc: &Document, x: u32, y: u32, editing: Option<NodeId>) -> [u8; 3] {
    let opts = RenderOptions { background: Some([255, 255, 255, 255]), isolated: editing, ..Default::default() };
    let p = Renderer::new().render(doc, 200, 200, Affine::IDENTITY, &opts).pixel(x, y);
    [p[0], p[1], p[2]]
}
fn bounds(s: &Session, id: NodeId) -> Rect {
    s.doc().unwrap().doc.node(id).unwrap().geometric_bounds().unwrap()
}

#[test]
fn powerclip_preserves_painted_frame_art_and_one_undo_step() {
    let mut s = session();
    let art = rect(&mut s, 0.0, 0.0, 80.0, 80.0, "#ff0000");
    let frame = rect(&mut s, 20.0, 20.0, 100.0, 100.0, "#0000ff");
    let before = s.doc().unwrap().doc.clone();
    let undo = s.doc().unwrap().history.undo.len();
    let id = NodeId(s.execute("object.powerClip.make", &json!({"frame":frame.0,"ids":[art.0]})).unwrap()["id"].as_u64().unwrap());
    let after = s.doc().unwrap().doc.clone();
    assert!(after.node(id).unwrap().is_powerclip());
    assert_eq!(after.node(frame).unwrap().appearance, before.node(frame).unwrap().appearance);
    assert_eq!(after.node(art), before.node(art));
    assert_eq!(s.doc().unwrap().history.undo.len(), undo + 1);
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc, before);
    s.execute("edit.redo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc, after);
}

#[test]
fn painted_frame_and_clipping_survive_native_svg_and_pdf() {
    let (s, id, frame, _) = setup();
    let doc = &s.doc().unwrap().doc;
    assert_eq!(rgb(doc, 70, 100, None), [255, 0, 0]);
    assert_eq!(rgb(doc, 120, 100, None), [0, 0, 255]);
    assert_eq!(rgb(doc, 45, 100, None), [0, 255, 0]);
    assert_eq!(rgb(doc, 52, 100, None), [0, 255, 0]);
    assert_eq!(rgb(doc, 30, 100, None), [255, 255, 255]);
    let back = vectorcraft_format::load(&vectorcraft_format::save(doc, false)).unwrap();
    assert_eq!(back, **doc);
    assert!(back.node(id).unwrap().extras().powerclip.unwrap().contents_locked);
    assert_eq!(back.node(frame).unwrap().id, frame);
    let svg = vectorcraft_svg::export(doc, &Default::default());
    let svg = vectorcraft_svg::import(&svg).unwrap();
    let pdf = vectorcraft_pdf::import(&vectorcraft_pdf::export(doc, &Default::default()).unwrap()).unwrap();
    for back in [&svg, &pdf] {
        assert_eq!(rgb(back, 70, 100, None), [255, 0, 0]);
        assert_eq!(rgb(back, 120, 100, None), [0, 0, 255]);
        assert_eq!(rgb(back, 30, 100, None), [255, 255, 255]);
    }
}

#[test]
fn editing_reveals_outside_art_and_hits_it_but_save_and_export_stay_clipped() {
    let (mut s, id, frame, art) = setup();
    let before = s.doc().unwrap().doc.clone();
    let undo = s.doc().unwrap().history.undo.len();
    s.execute("object.powerClip.edit", &json!({"id":id.0})).unwrap();
    let st = s.doc().unwrap();
    assert_eq!(st.isolation, Some(id));
    assert_eq!(rgb(&st.doc, 42, 100, st.isolation), [255, 0, 0]);
    assert!(hit_test(&st.doc, Point::new(42.0, 100.0), HitOptions::default()).is_none());
    let hit = hit_test(&st.doc, Point::new(42.0, 100.0), HitOptions { scope: st.isolation, ..Default::default() }).unwrap();
    assert_eq!(hit.top_object(st.isolation), art);
    s.execute("select.all", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().selection.objects, vec![art]);
    assert!(!s.doc().unwrap().selection.contains(frame));
    assert_eq!(s.doc().unwrap().history.undo.len(), undo);
    assert_eq!(s.doc().unwrap().doc, before);
    let back = vectorcraft_format::load(&vectorcraft_format::save(&s.doc().unwrap().doc, false)).unwrap();
    assert_eq!(rgb(&back, 42, 100, None), [255, 255, 255]);
    s.execute("object.powerClip.finish", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().isolation, None);
    assert_eq!(s.doc().unwrap().selection.objects, vec![id]);
    assert_eq!(s.doc().unwrap().doc, before);
}

#[test]
fn editing_offscreen_frame_expands_ancestor_cull_bounds() {
    let mut s = session();
    let art = rect(&mut s, 20.0, 20.0, 60.0, 60.0, "#ff0000");
    let frame = rect(&mut s, 1000.0, 1000.0, 100.0, 100.0, "#0000ff");
    let id = NodeId(s.execute("object.powerClip.make", &json!({"frame":frame.0,"ids":[art.0]})).unwrap()["id"].as_u64().unwrap());
    s.execute("object.group", &json!({})).unwrap();
    assert_eq!(rgb(&s.doc().unwrap().doc, 40, 40, None), [255, 255, 255]);
    assert_eq!(rgb(&s.doc().unwrap().doc, 40, 40, Some(id)), [255, 0, 0]);
}

#[test]
fn empty_frame_accepts_new_art_and_nested_frames_finish_to_parent_scope() {
    let mut s = session();
    let outer_frame = rect(&mut s, 0.0, 0.0, 180.0, 180.0, "#ffffff");
    let outer = NodeId(s.execute("object.powerClip.make", &json!({"frame":outer_frame.0,"ids":[]})).unwrap()["id"].as_u64().unwrap());
    s.execute("object.powerClip.edit", &json!({"id":outer.0})).unwrap();
    let inner_frame = rect(&mut s, 40.0, 40.0, 80.0, 80.0, "#0000ff");
    let inner = NodeId(s.execute("object.powerClip.make", &json!({"frame":inner_frame.0,"ids":[]})).unwrap()["id"].as_u64().unwrap());
    assert_eq!(s.doc().unwrap().doc.parent_of(inner), Some(outer));
    s.execute("object.powerClip.edit", &json!({"id":inner.0})).unwrap();
    let art = rect(&mut s, 30.0, 30.0, 100.0, 100.0, "#ff0000");
    assert_eq!(s.doc().unwrap().doc.parent_of(art), Some(inner));
    s.execute("object.powerClip.finish", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().isolation, Some(outer));
    s.execute("object.powerClip.finish", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().isolation, None);
    assert_eq!(rgb(&s.doc().unwrap().doc, 50, 50, None), [255, 0, 0]);
    assert_ne!(rgb(&s.doc().unwrap().doc, 35, 35, None), [255, 0, 0]);
}

#[test]
fn unlocked_frame_moves_alone_but_parent_group_moves_all_and_native_keeps_option() {
    let (mut s, id, frame, art) = setup();
    let (f, a) = (bounds(&s, frame), bounds(&s, art));
    s.execute("object.move", &json!({"dx":10,"dy":5})).unwrap();
    assert_eq!(bounds(&s, frame), f + vectorcraft_geom::Vec2::new(10.0, 5.0));
    assert_eq!(bounds(&s, art), a + vectorcraft_geom::Vec2::new(10.0, 5.0));
    s.execute("object.powerClip.lockContents", &json!({"id":id.0,"locked":false})).unwrap();
    let (f, a) = (bounds(&s, frame), bounds(&s, art));
    s.execute("object.move", &json!({"dx":15,"dy":0})).unwrap();
    assert_eq!(bounds(&s, frame), f + vectorcraft_geom::Vec2::new(15.0, 0.0));
    assert_eq!(bounds(&s, art), a);
    let back = vectorcraft_format::load(&vectorcraft_format::save(&s.doc().unwrap().doc, false)).unwrap();
    assert!(!back.node(id).unwrap().extras().powerclip.unwrap().contents_locked);
    s.execute("object.group", &json!({})).unwrap();
    s.execute("object.move", &json!({"dx":20,"dy":0})).unwrap();
    assert_eq!(bounds(&s, art), a + vectorcraft_geom::Vec2::new(20.0, 0.0));
}

#[test]
fn fit_fill_stretch_center_keep_all_content_relative_and_undo() {
    for (mode, w, h) in [("center", 40.0, 20.0), ("fit", 100.0, 50.0), ("fill", 200.0, 100.0), ("stretch", 100.0, 100.0)] {
        let mut s = session();
        let art = rect(&mut s, 0.0, 0.0, 40.0, 20.0, "#ff0000");
        let frame = rect(&mut s, 50.0, 50.0, 100.0, 100.0, "#ffffff");
        let id = NodeId(s.execute("object.powerClip.make", &json!({"frame":frame.0,"ids":[art.0]})).unwrap()["id"].as_u64().unwrap());
        let before = s.doc().unwrap().doc.clone();
        s.execute("object.powerClip.fit", &json!({"id":id.0,"mode":mode})).unwrap();
        let b = bounds(&s, art);
        assert!((b.width() - w).abs() < 1e-7 && (b.height() - h).abs() < 1e-7, "{mode}: {b:?}");
        assert_eq!(b.center(), Point::new(100.0, 100.0));
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(s.doc().unwrap().doc, before);
    }
}

#[test]
fn extract_returns_original_frame_and_contents_at_world_positions_and_undo_restores() {
    let (mut s, id, frame, art) = setup();
    s.execute("object.transform", &json!({"ids":[id.0],"matrix":[0,1,-1,0,180,0]})).unwrap();
    let before = s.doc().unwrap().doc.clone();
    let (f, a) = (bounds(&s, frame), bounds(&s, art));
    s.execute("object.powerClip.extract", &json!({"id":id.0})).unwrap();
    assert!(s.doc().unwrap().doc.node(id).is_none());
    assert_eq!(bounds(&s, frame), f);
    assert_eq!(bounds(&s, art), a);
    assert!(matches!(s.doc().unwrap().doc.node(frame).unwrap().kind, NodeKind::Path { clipping: false, .. }));
    assert_eq!(s.doc().unwrap().doc.node(frame).unwrap().appearance, before.node(frame).unwrap().appearance);
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc, before);
}

#[test]
fn compound_frame_keeps_its_hole() {
    let mut s = session();
    let art = rect(&mut s, 0.0, 0.0, 200.0, 200.0, "#ff0000");
    let outer = rect(&mut s, 20.0, 20.0, 160.0, 160.0, "#0000ff");
    let hole = rect(&mut s, 70.0, 70.0, 60.0, 60.0, "#0000ff");
    s.execute("select.set", &json!({"ids":[outer.0,hole.0]})).unwrap();
    let frame = NodeId(s.execute("object.compoundPath.make", &json!({})).unwrap()["id"].as_u64().unwrap());
    s.execute("object.powerClip.make", &json!({"frame":frame.0,"ids":[art.0]})).unwrap();
    assert_eq!(rgb(&s.doc().unwrap().doc, 50, 50, None), [255, 0, 0]);
    assert_eq!(rgb(&s.doc().unwrap().doc, 100, 100, None), [255, 255, 255]);
}

#[test]
fn fitting_uses_rotated_frame_axes_and_moves_multiple_contents_as_one_arrangement() {
    let mut s = session();
    let a = rect(&mut s, 0.0, 0.0, 10.0, 20.0, "#ff0000");
    let b = rect(&mut s, 30.0, 0.0, 10.0, 20.0, "#ff0000");
    let frame = rect(&mut s, 50.0, 50.0, 100.0, 50.0, "#0000ff");
    s.execute("object.transform", &json!({"ids":[frame.0],"matrix":[0,1,-1,0,180,0]})).unwrap();
    let id = NodeId(s.execute("object.powerClip.make", &json!({"frame":frame.0,"ids":[a.0,b.0]})).unwrap()["id"].as_u64().unwrap());
    // The contents measured along the rotated frame's own axes: a quarter turn swaps their
    // 40 × 20 arrangement to 20 × 40 there.
    let local_boxes = |s: &Session| {
        let d = &s.doc().unwrap().doc;
        let room = d.oriented_bounds(&[frame], false).unwrap();
        let local = room.to_doc().inverse();
        let symbols = d.symbol_boxes();
        let aa = d.node(a).unwrap().bounds_in(&symbols, local, false).unwrap();
        let bb = d.node(b).unwrap().bounds_in(&symbols, local, false).unwrap();
        (room.rect, aa, bb)
    };
    let (_, a0, b0) = local_boxes(&s);
    let before = a0.union(b0);
    assert!((before.width() - 20.0).abs() < 1e-7 && (before.height() - 40.0).abs() < 1e-7, "{before:?}");
    s.execute("object.powerClip.fit", &json!({"id":id.0,"mode":"fit"})).unwrap();
    let (room, aa, bb) = local_boxes(&s);
    let union = aa.union(bb);
    // One scale for the whole arrangement: its proportions are kept and it fits the frame.
    assert!((union.width() / before.width() - union.height() / before.height()).abs() < 1e-7);
    assert!(union.width() <= room.width() + 1e-7 && union.height() <= room.height() + 1e-7);
    assert!((union.width() - room.width()).abs() < 1e-7 || (union.height() - room.height()).abs() < 1e-7);
    assert!((union.center() - room.center()).hypot() < 1e-7);
    assert!((aa.width() - bb.width()).abs() < 1e-7);
}

#[test]
fn failed_cycle_and_invalid_fit_params_preserve_document_and_history() {
    let (mut s, id, _, art) = setup();
    let before = s.doc().unwrap().doc.clone();
    let steps = s.doc().unwrap().history.undo.len();
    for (command, params) in [
        ("object.powerClip.make", json!({"frame":id.0,"ids":[art.0]})),
        ("object.powerClip.make", json!({"frame":id.0,"ids":[id.0,id.0]})),
        ("object.powerClip.fit", json!({"id":id.0,"mode":"invalid"})),
        ("object.powerClip.lockContents", json!({"id":id.0,"locked":"false"})),
    ] {
        assert!(s.execute(command, &params).is_err());
        assert_eq!(s.doc().unwrap().doc, before);
        assert_eq!(s.doc().unwrap().history.undo.len(), steps);
    }
}

#[test]
fn unlocked_frame_preview_and_undo_do_not_move_contents() {
    let (mut s, id, frame, art) = setup();
    s.execute("object.powerClip.lockContents", &json!({"id":id.0,"locked":false})).unwrap();
    let before = s.doc().unwrap().doc.clone();
    let a = bounds(&s, art);
    let f = bounds(&s, frame);
    s.begin_interaction("Move").unwrap();
    s.preview("object.transform", &json!({"matrix":[1,0,0,1,15,5]})).unwrap();
    s.preview("object.transform", &json!({"matrix":[1,0,0,1,30,10]})).unwrap();
    s.commit_interaction().unwrap();
    assert_eq!(bounds(&s, art), a);
    assert_eq!(bounds(&s, frame), f + vectorcraft_geom::Vec2::new(30.0, 10.0));
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc, before);
}
