//! Restore the explicit PowerClip hierarchy emitted by VectorCraft's patched CDR backend.
//! Source identities come from CDR records, never from matching positions or colours.

use std::sync::Arc;
use vectorcraft_color::BlendMode;
use vectorcraft_doc::{Document, Node, NodeKind, PowerClipSpec};

const PREFIX: &str = "VC_POWERCLIP_";
const FRAME_PREFIX: &str = "VC_POWERCLIP_FRAME_";
const CONTENT_PREFIX: &str = "VC_POWERCLIP_CONTENT_";
const INVALID: &str = "CDR PowerClip frame could not be imported as an editable clipping shape";
const MAX_DEPTH: usize = 512;

fn failure() -> String {
    INVALID.to_string()
}

/// A named frame wrapper keeps source paint distinct from its SVG clip geometry. A plain
/// one-child wrapper can be folded, just as the SVG importer folds unnamed shape wrappers.
fn frame_shape(mut frame: Node) -> Result<Node, String> {
    for _ in 0..MAX_DEPTH {
        if matches!(frame.kind, NodeKind::Path { guide: false, .. } | NodeKind::Compound { .. } | NodeKind::Text(_)) {
            return Ok(frame);
        }
        if frame.extras().mask.is_some() || !frame.appearance.effects.is_empty() {
            return Err(failure());
        }
        let only = match &mut frame.kind {
            NodeKind::Group { children, clip: false } if children.len() == 1 => children.pop().ok_or_else(failure)?,
            _ => return Err(failure()),
        };
        let mut child = Arc::unwrap_or_clone(only);
        if frame.blend != BlendMode::Normal && child.blend != BlendMode::Normal {
            return Err(failure());
        }
        child.opacity *= frame.opacity;
        child.visible &= frame.visible;
        child.isolate |= frame.isolate;
        if frame.blend != BlendMode::Normal {
            child.blend = frame.blend;
        }
        frame = child;
    }
    Err(failure())
}

fn restore_node(node: &mut Node, depth: usize) -> Result<usize, String> {
    if depth > MAX_DEPTH {
        return Err(failure());
    }
    let mut count = 0;
    if let Some(children) = node.children_mut() {
        for child in children {
            count += restore_node(Arc::make_mut(child), depth + 1)?;
        }
    }
    let source = node.name.as_deref().and_then(|n| n.strip_prefix(PREFIX)).and_then(|n| n.split('_').next()).and_then(|n| n.parse::<u32>().ok());
    let Some(source) = source else { return Ok(count) };
    let NodeKind::Group { children, clip: false } = &mut node.kind else { return Err(failure()) };
    // The backend paints the frame outside a separate clipped content wrapper. Promote
    // that wrapper's real clipPath into our native frame, retaining its contents and style.
    let content_index = children.iter().position(|n| n.name.as_deref().is_some_and(|s| s.starts_with(CONTENT_PREFIX)));
    let mut generated_frame = None;
    let mut content = None;
    if let Some(index) = content_index {
        let mut group = Arc::unwrap_or_clone(children.remove(index));
        let NodeKind::Group { children: items, clip } = &mut group.kind else { return Err(failure()) };
        if !*clip || items.is_empty() {
            return Err(failure());
        }
        generated_frame = Some(Arc::unwrap_or_clone(items.remove(0)));
        *clip = false;
        if !items.is_empty() {
            // Keep this wrapper: opacity, blending and masks belong to the contents,
            // rather than to the frame or to each individual child.
            content = Some(Arc::new(group));
        }
    }
    let painted_index = children.iter().position(|n| n.name.as_deref().is_some_and(|s| s.starts_with(FRAME_PREFIX)));
    let mut frame = if let Some(index) = painted_index {
        frame_shape(Arc::unwrap_or_clone(children.remove(index)))?
    } else {
        frame_shape(generated_frame.ok_or_else(failure)?)?
    };
    if !children.is_empty() {
        return Err(failure());
    }
    crate::cmd::powerclip::mark_frame(&mut frame);
    let mut native_children = vec![Arc::new(frame)];
    if let Some(group) = content {
        native_children.push(group);
    }
    node.kind = NodeKind::Group { children: native_children, clip: true };
    node.extras_mut().powerclip = Some(PowerClipSpec::default());
    node.edit_attrs(|a| {
        a.data.retain(|(k, _)| k != "cdr-powerclip-source");
        a.data.push(("cdr-powerclip-source".into(), source.to_string()));
    });
    node.name = Some(format!("PowerClip {source}"));
    Ok(count + 1)
}

/// Preserve native masks, frame paint and nested content through native save and later edits.
pub(super) fn restore(doc: &mut Document) -> Result<usize, String> {
    let mut count = 0;
    for layer in &mut doc.layers {
        count += restore_node(Arc::make_mut(layer), 0)?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vectorcraft_geom::Affine;
    use vectorcraft_render::{RenderOptions, Renderer};

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200pt" height="200pt" viewBox="0 0 200 200">
<defs><clipPath id="pc42"><rect x="50" y="50" width="100" height="100"/></clipPath></defs>
<g id="Layer1000"><g id="VC_POWERCLIP_42" data-cdr-powerclip-raw="7,1,0,1">
 <g id="VC_POWERCLIP_FRAME_42"><rect x="50" y="50" width="100" height="100" fill="#0000ff" stroke="#00ff00" stroke-width="10"/></g>
 <g id="VC_POWERCLIP_CONTENT_42" clip-path="url(#pc42)"><rect x="40" y="50" width="60" height="100" fill="#ff0000"/></g>
</g></g></svg>"##;

    fn rgb(doc: &Document, x: u32) -> [u8; 3] {
        let opts = RenderOptions { background: Some([255, 255, 255, 255]), ..Default::default() };
        let p = Renderer::new().render(doc, 200, 200, Affine::IDENTITY, &opts).pixel(x, 100);
        [p[0], p[1], p[2]]
    }

    #[test]
    fn cdr_powerclip_restores_frame_paint_and_unclipped_outline_without_flattening_content() {
        let mut doc = vectorcraft_svg::import(SVG).unwrap();
        assert_eq!(restore(&mut doc).unwrap(), 1);
        assert_eq!(rgb(&doc, 70), [255, 0, 0], "editable content clips inside the frame");
        assert_eq!(rgb(&doc, 120), [0, 0, 255], "frame fill remains behind the content");
        assert_eq!(rgb(&doc, 45), [0, 255, 0], "frame outline remains visible outside the clipping region");
        assert_eq!(rgb(&doc, 30), [255, 255, 255], "outside contents remain clipped");
        let mut groups = vec![];
        doc.walk(|n| {
            if n.is_powerclip() {
                groups.push(n.id);
            }
        });
        let clip = doc.node(groups[0]).unwrap();
        assert_eq!(clip.children().unwrap().len(), 2, "visual frame must not be duplicated as content");
        assert_eq!(clip.extras().powerclip.unwrap(), PowerClipSpec::default());
        assert!(clip.extras().attrs.as_ref().unwrap().data.iter().any(|(k, v)| k == "cdr-powerclip-raw" && v == "7,1,0,1"));
        let native = vectorcraft_format::save_file(&doc);
        let reopened = vectorcraft_format::load_file(&native).unwrap().doc;
        assert!(reopened.node(groups[0]).unwrap().is_powerclip());
        assert_eq!(rgb(&reopened, 45), [0, 255, 0]);
    }

    #[test]
    fn cdr_powerclip_invisible_frame_uses_real_clip_geometry_and_keeps_nested_masks() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100pt" height="100pt" viewBox="0 0 100 100">
<defs><clipPath id="outer"><rect x="10" y="10" width="80" height="80"/></clipPath><clipPath id="inner"><rect x="20" y="20" width="30" height="30"/></clipPath></defs>
<g id="Layer1000"><g id="VC_POWERCLIP_1"><g id="VC_POWERCLIP_FRAME_1"><rect x="10" y="10" width="80" height="80" fill="none" stroke="none"/></g>
<g id="VC_POWERCLIP_CONTENT_1" clip-path="url(#outer)"><g id="VC_POWERCLIP_2_1"><g id="VC_POWERCLIP_CONTENT_2_1" clip-path="url(#inner)"><rect width="100" height="100" fill="#ff0000"/></g></g></g></g></g></svg>"##;
        let mut doc = vectorcraft_svg::import(svg).unwrap();
        assert_eq!(restore(&mut doc).unwrap(), 2);
        let mut count = 0;
        doc.walk(|n| {
            if n.is_powerclip() {
                count += 1;
                assert!(n.clips());
                assert!(!n.children().unwrap().is_empty());
            }
        });
        assert_eq!(count, 2);
        let opts = RenderOptions { background: Some([255, 255, 255, 255]), ..Default::default() };
        let image = Renderer::new().render(&doc, 100, 100, Affine::IDENTITY, &opts);
        assert_eq!(image.pixel(30, 30), [255, 0, 0, 255]);
        assert_eq!(image.pixel(70, 30), [255, 255, 255, 255]);
    }

    #[test]
    fn cdr_powerclip_marker_without_clipping_geometry_fails_without_guessing_another_shape() {
        let mut doc = vectorcraft_svg::import(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><g id="Layer1000"><g id="VC_POWERCLIP_9"><rect width="50" height="50"/></g></g></svg>"#).unwrap();
        assert!(restore(&mut doc).is_err());
        let mut ordinary = vectorcraft_svg::import(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><g id="Layer1000"><g id="ordinary"><rect width="50" height="50"/></g></g></svg>"#).unwrap();
        assert_eq!(restore(&mut ordinary).unwrap(), 0);
    }
}
