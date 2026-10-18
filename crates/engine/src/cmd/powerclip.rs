//! Editable PowerClip frames. Their geometry is an ordinary clipping group; editing scope is
//! transient, so saving or exporting while editing never exposes the retained outside art.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Value, json};
use vectorcraft_doc::{Document, Node, NodeId, NodeKind, PowerClipSpec, Scaling};
use vectorcraft_geom::{Affine, Rect, union_opt};

use super::{CommandSpec, bad, checked_id, checked_ids_or_selection, cmd, has_doc};
use crate::{DocState, EngineError, Result, Session};

const MAKE: &str = "object.powerClip.make";
const FIT: &str = "object.powerClip.fit";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "object.powerClip.make",
            "Place Inside Frame",
            ["Object", "PowerClip"],
            None,
            "{frame?: id (default: the topmost selected object), ids?: [ids] (default: the selection), center?: false} put editable contents inside a painted path, compound path or text frame; a frame alone creates an empty PowerClip, an existing PowerClip accepts more contents. Keeps geometry and frame paint; no destructive crop; one undo step → {id, frame, contents}",
            can_make,
            make
        ),
        cmd!(query "object.powerClip.info", "PowerClip", [], None,
            "{id?: frame | PowerClip | object inside one} → {id, frame, contents, contentsLocked, editing} or null", has_doc, info),
        cmd!(
            "object.powerClip.edit",
            "Edit Contents",
            ["Object", "PowerClip"],
            None,
            "{id?: frame | PowerClip | object inside one} edit the retained contents, including outside the frame, in isolation; does not change saved/exported clipping",
            can_powerclip,
            edit
        ),
        cmd!(
            "object.powerClip.finish",
            "Finish Editing",
            ["Object", "PowerClip"],
            None,
            "{} finish PowerClip content editing and select its frame container",
            can_finish,
            finish
        ),
        cmd!(
            "object.powerClip.selectFrame",
            "Select Frame",
            ["Object", "PowerClip"],
            None,
            "{id?: frame | PowerClip | object inside one} leave content editing and select the painted clipping frame",
            can_powerclip,
            |s, p| select(s, p, true)
        ),
        cmd!(
            "object.powerClip.selectContents",
            "Select Contents",
            ["Object", "PowerClip"],
            None,
            "{id?: frame | PowerClip | object inside one} enter content editing and select its retained objects",
            can_powerclip,
            |s, p| select(s, p, false)
        ),
        cmd!(
            "object.powerClip.extract",
            "Extract Contents",
            ["Object", "PowerClip"],
            None,
            "{id?: frame | PowerClip | object inside one} remove the clipping relationship, keep frame and contents at their document positions and preserve paint; a container with transparency/effects stays as an ordinary group to preserve its appearance; one undo step → {frame, contents, group?}",
            can_powerclip,
            extract
        ),
        cmd!(
            "object.powerClip.lockContents",
            "Lock Contents",
            ["Object", "PowerClip"],
            None,
            "{id?: frame | PowerClip | object inside one, locked?: bool (default: toggle)} locked: moving/resizing the selected PowerClip takes contents with its frame; unlocked: ordinary transforms of the selected PowerClip move its frame alone; explicit content edits remain possible",
            can_powerclip,
            lock
        ),
        cmd!(
            "object.powerClip.fit",
            "Fit Contents",
            ["Object", "PowerClip"],
            None,
            "{id?: frame | PowerClip | object inside one, mode: center|fit|fill|stretch} center without resizing; fit/fill preserve proportions, stretch matches both frame dimensions; use the frame's rotated axes and keep the relative arrangement of all retained contents; one undo step",
            can_fit,
            fit
        ),
    ]
}

/// The PowerClip currently being edited, if its transient isolation target still exists.
pub fn editing(st: &DocState) -> Option<NodeId> {
    st.isolation.filter(|id| st.doc.node(*id).is_some_and(Node::is_powerclip))
}

/// The innermost PowerClip selected by its container, frame or any content; editing an empty
/// frame also works with no art selected. Shared by thin UI controls and engine commands.
pub fn selected(s: &Session) -> Option<NodeId> {
    let st = s.active()?;
    st.selection.objects.iter().find_map(|id| enclosing(&st.doc, *id)).or_else(|| editing(st))
}

fn enclosing(d: &Document, id: NodeId) -> Option<NodeId> {
    d.ancestry(id)?.into_iter().rev().find(|id| d.node(*id).is_some_and(Node::is_powerclip))
}

fn can_powerclip(s: &Session) -> std::result::Result<(), String> {
    has_doc(s)?;
    selected(s).map(|_| ()).ok_or_else(|| "select a PowerClip frame or its contents".into())
}

fn can_make(s: &Session) -> std::result::Result<(), String> {
    has_doc(s)?;
    let ids = super::edit::selected_roots(s).map_err(|e| e.to_string())?;
    ids.last()
        .and_then(|id| s.active()?.doc.node(*id))
        .filter(|n| is_frame(n) || n.is_powerclip())
        .map(|_| ())
        .ok_or_else(|| "select a frame; put it above the contents".into())
}

fn can_fit(s: &Session) -> std::result::Result<(), String> {
    can_powerclip(s)?;
    let has = s.active().and_then(|st| st.doc.node(selected(s)?)).and_then(Node::children).is_some_and(|c| c.len() > 1);
    if has { Ok(()) } else { Err("the PowerClip frame has no contents to fit".into()) }
}

fn can_finish(s: &Session) -> std::result::Result<(), String> {
    s.active().and_then(editing).map(|_| ()).ok_or_else(|| "no PowerClip contents are being edited".into())
}

fn target(s: &Session, p: &Value, command: &str) -> Result<NodeId> {
    let id = match p.get("id") {
        Some(v) => enclosing(&s.doc()?.doc, checked_id(s, v, command)?),
        None => selected(s),
    }
    .ok_or_else(|| bad(command, "select a PowerClip frame or its contents"))?;
    if !s.doc()?.doc.is_editable(id) {
        return Err(bad(command, "the PowerClip frame is locked or hidden"));
    }
    Ok(id)
}

fn parts(d: &Document, id: NodeId) -> Result<(NodeId, Vec<NodeId>)> {
    let children = d
        .node(id)
        .filter(|n| n.is_powerclip())
        .and_then(Node::children)
        .ok_or_else(|| bad("object.powerClip.info", "select a PowerClip frame or its contents"))?;
    let frame = children.first().ok_or_else(|| bad("object.powerClip.info", "the PowerClip has no clipping frame"))?;
    Ok((frame.id, children.iter().skip(1).map(|n| n.id).collect()))
}

fn info(s: &mut Session, p: &Value) -> Result<Value> {
    let id = if p.get("id").is_some() { Some(target(s, p, "object.powerClip.info")?) } else { selected(s) };
    let Some(id) = id else { return Ok(Value::Null) };
    let st = s.doc()?;
    let (frame, contents) = parts(&st.doc, id)?;
    let locked = st.doc.node(id).and_then(|n| n.extras().powerclip).is_none_or(|spec| spec.contents_locked);
    Ok(
        json!({"id":id.0,"frame":frame.0,"contents":contents.iter().map(|id|id.0).collect::<Vec<_>>(),"contentsLocked":locked,"editing":editing(st)==Some(id)}),
    )
}

/// A path, compound path or type: what can frame a PowerClip (Place Inside Frame, imported PowerClips).
pub(crate) fn is_frame(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Path { guide: false, .. } | NodeKind::Compound { .. } | NodeKind::Text(_))
}

/// Preserve paint: unlike Make Clipping Mask, a PowerClip's frame remains filled and stroked.
/// Shared by Place Inside Frame and the importers that make PowerClips (CorelDRAW).
pub(crate) fn mark_frame(n: &mut Node) {
    if let NodeKind::Path { clipping, .. } = &mut n.kind {
        *clipping = true;
    }
}

fn make(s: &mut Session, p: &Value) -> Result<Value> {
    let mut ids = checked_ids_or_selection(s, p, MAKE)?;
    if ids.len() > 10_000 {
        return Err(bad(MAKE, "a PowerClip accepts at most 10000 objects per operation"));
    }
    let d = &s.doc()?.doc;
    // Tree order, rather than click order, keeps overlapping content in its original paint order.
    let wanted: HashSet<NodeId> = ids.iter().copied().collect();
    if wanted.len() != ids.len() {
        return Err(bad(MAKE, "PowerClip object ids must be distinct"));
    }
    ids.clear();
    d.walk(|n| {
        if wanted.contains(&n.id) {
            ids.push(n.id);
        }
    });
    let frame = match p.get("frame") {
        Some(v) => checked_id(s, v, MAKE)?,
        None => ids.last().copied().ok_or_else(|| bad(MAKE, "select a frame; put it above the contents"))?,
    };
    ids.retain(|id| *id != frame);
    let n = d.node(frame).ok_or(EngineError::NoNode(frame))?;
    let existing = matches!(n.kind, NodeKind::Group { clip: true, .. });
    if !existing && !is_frame(n) {
        return Err(bad(MAKE, "the PowerClip frame must be a path, compound path or text"));
    }
    if existing && n.children().is_none_or(|c| c.first().is_none_or(|c| !is_frame(c))) {
        return Err(bad(MAKE, "the PowerClip has no valid clipping frame"));
    }
    for id in ids.iter().copied().chain(std::iter::once(frame)) {
        if !d.is_editable(id) {
            return Err(bad(MAKE, "PowerClip objects must be visible and unlocked"));
        }
        if d.node(id).is_some_and(Node::is_layer) {
            return Err(bad(MAKE, "a layer cannot be PowerClip contents"));
        }
    }
    let roots: HashSet<NodeId> = ids.iter().copied().chain(std::iter::once(frame)).collect();
    if roots.iter().any(|id| d.ancestry(*id).is_some_and(|a| a.into_iter().any(|ancestor| ancestor != *id && roots.contains(&ancestor)))) {
        return Err(bad(MAKE, "a PowerClip cannot contain its own frame or an ancestor"));
    }
    let center = match p.get("center") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(MAKE, "center must be true or false"))?,
    };
    let gid = s.edit("Place Inside Frame", |d, sel| {
        let gid = if existing {
            let nodes = d.remove_many(&ids)?;
            let group = d.node_mut(frame).ok_or(EngineError::NoNode(frame))?;
            if group.extras().powerclip.is_none() {
                group.extras_mut().powerclip = Some(PowerClipSpec::default());
            }
            group.children_mut().ok_or(EngineError::NoNode(frame))?.extend(nodes);
            frame
        } else {
            mark_frame(d.node_mut(frame).ok_or(EngineError::NoNode(frame))?);
            let gid = d.alloc_id();
            let mut wrap = ids.clone();
            wrap.push(frame);
            d.wrap(&wrap, |mut children| {
                children.rotate_right(1);
                let mut n = Node::new(gid, NodeKind::Group { children, clip: true });
                n.extras_mut().powerclip = Some(PowerClipSpec::default());
                n
            })?
        };
        if center && !ids.is_empty() {
            fit_in(d, gid, "center")?;
        }
        sel.set([gid]);
        Ok(gid)
    })?;
    // Existing isolation must not swallow the newly selected container.
    if s.doc()?.isolation == Some(gid) {
        s.doc_mut()?.isolation = None;
    }
    info(s, &json!({"id":gid.0}))
}

fn edit(s: &mut Session, p: &Value) -> Result<Value> {
    let id = target(s, p, "object.powerClip.edit")?;
    let (_, contents) = parts(&s.doc()?.doc, id)?;
    let st = s.doc_mut()?;
    st.isolation = Some(id);
    st.selection.set(contents);
    st.revision += 1;
    Ok(json!({"id":id.0}))
}

pub(super) fn finish(s: &mut Session, _: &Value) -> Result<Value> {
    let st = s.doc_mut()?;
    let id = editing(st).ok_or_else(|| bad("object.powerClip.finish", "no PowerClip contents are being edited"))?;
    // Return to an enclosing PowerClip's edit scope when finishing a nested frame.
    st.isolation = st.doc.parent_of(id).and_then(|p| enclosing(&st.doc, p));
    st.selection.set([id]);
    st.revision += 1;
    Ok(json!({"id":id.0}))
}

fn select(s: &mut Session, p: &Value, frame: bool) -> Result<Value> {
    if !frame {
        return edit(s, p);
    }
    let id = target(s, p, "object.powerClip.selectFrame")?;
    let (frame, _) = parts(&s.doc()?.doc, id)?;
    let st = s.doc_mut()?;
    st.isolation = st.doc.parent_of(id).and_then(|p| enclosing(&st.doc, p));
    st.selection.set([frame]);
    st.revision += 1;
    Ok(json!({"id":frame.0}))
}

fn lock(s: &mut Session, p: &Value) -> Result<Value> {
    let id = target(s, p, "object.powerClip.lockContents")?;
    let old = s.doc()?.doc.node(id).and_then(|n| n.extras().powerclip).unwrap_or_default();
    let locked = match p.get("locked") {
        None => !old.contents_locked,
        Some(v) => v.as_bool().ok_or_else(|| bad("object.powerClip.lockContents", "locked must be true or false"))?,
    };
    if old.contents_locked != locked {
        s.edit("Lock Contents", |d, _| {
            d.node_mut(id).ok_or(EngineError::NoNode(id))?.extras_mut().powerclip = Some(PowerClipSpec { contents_locked: locked });
            Ok(())
        })?;
    }
    Ok(json!({"id":id.0,"contentsLocked":locked}))
}

/// Transform a selected object as an editing action. Generic Node::transform deliberately keeps
/// moving all geometry, so import placement, copying and export calculations ignore this lock.
pub(crate) fn transform_object(n: &mut Node, xf: Affine, scaling: Scaling) {
    if n.is_powerclip() && n.extras().powerclip.is_some_and(|spec| !spec.contents_locked) {
        if let Some(frame) = n.children_mut().and_then(|c| c.first_mut()) {
            Arc::make_mut(frame).transform(xf, scaling);
            n.bbox_angle = vectorcraft_geom::normalize_deg(vectorcraft_doc::orient::transformed_angle(n.bbox_angle, xf));
        }
    } else {
        n.transform(xf, scaling);
    }
}

fn fit(s: &mut Session, p: &Value) -> Result<Value> {
    let id = target(s, p, FIT)?;
    let mode = p
        .get("mode")
        .and_then(Value::as_str)
        .filter(|m| matches!(*m, "center" | "fit" | "fill" | "stretch"))
        .ok_or_else(|| bad(FIT, "mode must be center, fit, fill or stretch"))?;
    let xf = fit_transform(&s.doc()?.doc, id, mode)?;
    if xf != Affine::IDENTITY {
        s.edit("Fit Contents", |d, _| {
            transform_contents(d, id, xf)?;
            Ok(())
        })?;
    }
    Ok(json!({"id":id.0,"mode":mode}))
}

fn fit_in(d: &mut Document, id: NodeId, mode: &str) -> Result<()> {
    let xf = fit_transform(d, id, mode)?;
    transform_contents(d, id, xf)
}

fn fit_transform(d: &Document, id: NodeId, mode: &str) -> Result<Affine> {
    let n = d.node(id).ok_or(EngineError::NoNode(id))?;
    let (frame, contents) = n.children().and_then(|c| c.split_first()).ok_or_else(|| bad(FIT, "the PowerClip has no clipping frame"))?;
    if contents.is_empty() {
        return Err(bad(FIT, "the PowerClip frame has no contents to fit"));
    }
    let symbols = d.symbol_boxes();
    let room = d.oriented_bounds(&[frame.id], false).ok_or_else(|| bad(FIT, "the PowerClip frame has no usable bounds"))?;
    let local = room.to_doc().inverse();
    let bounds = contents
        .iter()
        .fold(None, |b, c| union_opt(b, c.bounds_in(&symbols, local, false)))
        .ok_or_else(|| bad(FIT, "the PowerClip contents have no usable bounds"))?;
    let valid = |b: Rect| [b.x0, b.y0, b.x1, b.y1].iter().all(|x| x.is_finite()) && b.width() > 1e-9 && b.height() > 1e-9;
    if !valid(room.rect) || !valid(bounds) {
        return Err(bad(FIT, "the PowerClip frame and contents must have nonzero finite bounds"));
    }
    let (sx, sy) = match mode {
        "center" => (1.0, 1.0),
        "fit" => {
            let k = (room.rect.width() / bounds.width()).min(room.rect.height() / bounds.height());
            (k, k)
        }
        "fill" => {
            let k = (room.rect.width() / bounds.width()).max(room.rect.height() / bounds.height());
            (k, k)
        }
        "stretch" => (room.rect.width() / bounds.width(), room.rect.height() / bounds.height()),
        _ => return Err(bad(FIT, "mode must be center, fit, fill or stretch")),
    };
    let xf = room.conjugate(
        Affine::translate(room.rect.center().to_vec2()) * Affine::scale_non_uniform(sx, sy) * Affine::translate(-bounds.center().to_vec2()),
    );
    if !xf.as_coeffs().iter().all(|x| x.is_finite() && x.abs() <= crate::MAX_COORD) {
        return Err(bad(FIT, "the PowerClip fit transform is outside the supported coordinate range"));
    }
    Ok(xf)
}

fn transform_contents(d: &mut Document, id: NodeId, xf: Affine) -> Result<()> {
    let children = d.node_mut(id).and_then(Node::children_mut).ok_or(EngineError::NoNode(id))?;
    for child in children.iter_mut().skip(1) {
        Arc::make_mut(child).transform(xf, Scaling::default());
    }
    Ok(())
}

fn extract(s: &mut Session, p: &Value) -> Result<Value> {
    let id = target(s, p, "object.powerClip.extract")?;
    let (frame, contents) = parts(&s.doc()?.doc, id)?;
    let group = s.edit("Extract Contents", |d, sel| {
        let mut group = d.node(id).cloned().ok_or(EngineError::NoNode(id))?;
        let mut children = group.children().cloned().unwrap_or_default();
        if let Some(frame) = children.first_mut() {
            let frame = Arc::make_mut(frame);
            if let NodeKind::Path { clipping, .. } = &mut frame.kind {
                *clipping = false;
            }
        }
        group.extras_mut().powerclip = None;
        let keep = !group.has_default_transparency() || !group.appearance.items.is_empty() || group.extras().wrap.is_some();
        let out = if keep {
            group.kind = NodeKind::Group { children, clip: false };
            vec![group]
        } else {
            children.into_iter().map(|c| (*c).clone()).collect()
        };
        d.insert_beside(vec![(id, out)], true)?;
        sel.set(std::iter::once(frame).chain(contents.iter().copied()));
        Ok(keep.then_some(id))
    })?;
    if s.doc()?.isolation == Some(id) {
        s.doc_mut()?.isolation = None;
    }
    Ok(json!({"frame":frame.0,"contents":contents.iter().map(|id|id.0).collect::<Vec<_>>(),"group":group.map(|id|id.0)}))
}
