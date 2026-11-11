//! Where each image is drawn: a walk over every page's content that finds
//! the image XObjects it draws and the resolution each is drawn at.
//!
//! The walk follows `q`, `Q` and `cm` into Form XObjects (their `/Matrix`
//! applied, ten levels deep at most). Images drawn nowhere it reaches
//! (annotation appearances, patterns, Type 3 glyphs) are not recorded, so
//! the image pass leaves them alone.

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};

/// A PDF matrix `[a b c d e f]`.
type Matrix = [f64; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// Form XObjects nest at most this deep.
const MAX_DEPTH: usize = 10;

/// A placement narrower or shorter than this, in points, draws nothing
/// anyone can see, so it says nothing about the resolution an image needs.
const MIN_EXTENT_PT: f64 = 0.01;

/// `m` then `n`: the matrix `cm` makes from operand `m` and the current
/// matrix `n`.
fn mul(m: &Matrix, n: &Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn number(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

/// Six numbers as a matrix, or `None` when the operands are not that.
fn matrix(operands: &[Object]) -> Option<Matrix> {
    if operands.len() != 6 {
        return None;
    }
    let mut m = [0.0; 6];
    for (slot, o) in m.iter_mut().zip(operands) {
        *slot = number(o)?;
    }
    m.iter().all(|v| v.is_finite()).then_some(m)
}

/// What the walk found.
#[derive(Debug, Default, Clone)]
pub(crate) struct Placements {
    /// Per image, the lowest resolution it is drawn at, in pixels per inch
    /// along its less dense axis. The lowest, not the highest: an image
    /// drawn both small and large needs its pixels for the large drawing.
    pub dpi: HashMap<ObjectId, f64>,
    /// Images some page lists in its resources but whose content could not
    /// be read, so where (and how large) that page draws them is unknown.
    pub unknown: HashSet<ObjectId>,
}

impl Placements {
    fn record(&mut self, id: ObjectId, dpi: f64) {
        self.dpi
            .entry(id)
            .and_modify(|d| *d = d.min(dpi))
            .or_insert(dpi);
    }
}

/// The resolution an image `width` x `height` pixels is drawn at under
/// `ctm`, in points scaled by `user_unit`: its less dense axis, or `None`
/// when the placement is too small to see.
fn placement_dpi(ctm: &Matrix, width: f64, height: f64, user_unit: f64) -> Option<f64> {
    let w_pt = ctm[0].hypot(ctm[1]) * user_unit;
    let h_pt = ctm[2].hypot(ctm[3]) * user_unit;
    if !(w_pt >= MIN_EXTENT_PT && h_pt >= MIN_EXTENT_PT) {
        return None;
    }
    let dx = width / (w_pt / 72.0);
    let dy = height / (h_pt / 72.0);
    Some(dx.min(dy))
}

/// `o` itself, or the object it refers to.
pub(crate) fn resolve<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    doc.dereference(o).ok().map(|(_, o)| o)
}

pub(crate) fn dict_of<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Dictionary> {
    match resolve(doc, o)? {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

/// A page's resources: its own, else the nearest ancestor's.
pub(crate) fn page_resources(doc: &Document, page: ObjectId) -> Option<&Dictionary> {
    let mut node = doc.get_dictionary(page).ok()?;
    for _ in 0..64 {
        if let Some(r) = node.get(b"Resources").ok().and_then(|o| dict_of(doc, o)) {
            return Some(r);
        }
        let parent = node.get(b"Parent").ok()?.as_reference().ok()?;
        node = doc.get_dictionary(parent).ok()?;
    }
    None
}

/// The `/XObject` entries of `resources`: name, object id.
pub(crate) fn xobjects(doc: &Document, resources: &Dictionary) -> Vec<(Vec<u8>, ObjectId)> {
    let Some(dict) = resources.get(b"XObject").ok().and_then(|o| dict_of(doc, o)) else {
        return Vec::new();
    };
    dict.iter()
        .filter_map(|(name, o)| o.as_reference().ok().map(|id| (name.clone(), id)))
        .collect()
}

fn subtype(doc: &Document, id: ObjectId) -> Option<(&[u8], &lopdf::Stream)> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let sub = stream.dict.get(b"Subtype").ok()?.as_name().ok()?;
    Some((sub, stream))
}

/// Content far larger than this, a page's or a form's, is not one conv can
/// walk in a reasonable time anyway.
const MAX_CONTENT: usize = 512 << 20;

/// Form XObjects drawn in the whole document, at most: forms that each draw
/// the next several times multiply, ten levels deep.
const MAX_FORM_DRAWS: usize = 100_000;

/// The decoded content of a stream, at most `limit` bytes, or `None` when
/// it cannot be read.
fn content_of(stream: &lopdf::Stream, limit: usize) -> Option<Vec<u8>> {
    if stream.dict.get(b"Filter").is_err() {
        return (stream.content.len() <= limit).then(|| stream.content.clone());
    }
    stream.decompressed_content_with_limit(limit).ok()
}

/// The concatenated content of a page, at most `limit` bytes, or `None`
/// when any part of it is unreadable. The parts share the limit, since a
/// page may list the same stream many times.
fn page_content(doc: &Document, page: ObjectId, limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for id in doc.get_page_contents(page) {
        let stream = doc.get_object(id).ok()?.as_stream().ok()?;
        out.extend(content_of(stream, limit.checked_sub(out.len())?)?);
        out.push(b'\n');
    }
    Some(out)
}

/// Every image XObject reachable from `resources` through Form XObjects,
/// drawn or not.
fn images_listed(
    doc: &Document,
    resources: &Dictionary,
    depth: usize,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) {
    for (_, id) in xobjects(doc, resources) {
        if !seen.insert(id) {
            continue;
        }
        match subtype(doc, id) {
            Some((b"Image", _)) => out.push(id),
            Some((b"Form", form)) if depth < MAX_DEPTH => {
                if let Some(r) = form
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|o| dict_of(doc, o))
                {
                    images_listed(doc, r, depth + 1, seen, out);
                }
            }
            _ => {}
        }
    }
}

/// Every image XObject the pages list in their resources (or in the
/// resources of the forms they list), each once, in page order.
pub(crate) fn listed_images(doc: &Document) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for page in doc.page_iter() {
        if let Some(r) = page_resources(doc, page) {
            images_listed(doc, r, 0, &mut seen, &mut out);
        }
    }
    out
}

struct Walker<'a> {
    doc: &'a Document,
    user_unit: f64,
    found: &'a mut Placements,
    /// Forms being walked, so one that draws itself is not walked forever.
    active: Vec<ObjectId>,
    /// Form draws left in the document's `MAX_FORM_DRAWS`.
    draws_left: &'a mut usize,
}

impl Walker<'_> {
    /// Walks one content stream drawn under `base` with `resources`.
    /// `false` when the content could not be parsed.
    fn walk(
        &mut self,
        data: &[u8],
        resources: Option<&Dictionary>,
        base: Matrix,
        depth: usize,
    ) -> bool {
        // Strict: the lenient decode stops at the first bad token and keeps
        // what came before, so later, larger drawings would go unseen.
        let Ok(content) = Content::decode_strict(data) else {
            return false;
        };
        let names: HashMap<Vec<u8>, ObjectId> = resources
            .map(|r| xobjects(self.doc, r).into_iter().collect())
            .unwrap_or_default();
        let mut stack: Vec<Matrix> = Vec::new();
        let mut ctm = base;
        for op in &content.operations {
            match op.operator.as_str() {
                "q" => stack.push(ctm),
                "Q" => ctm = stack.pop().unwrap_or(base),
                "cm" => {
                    if let Some(m) = matrix(&op.operands) {
                        ctm = mul(&m, &ctm);
                    }
                }
                "Do" => {
                    let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) else {
                        continue;
                    };
                    let Some(&id) = names.get(name) else {
                        continue;
                    };
                    self.draw(id, &ctm, resources, depth);
                }
                _ => {}
            }
        }
        true
    }

    fn draw(&mut self, id: ObjectId, ctm: &Matrix, parent: Option<&Dictionary>, depth: usize) {
        let doc = self.doc;
        match subtype(doc, id) {
            Some((b"Image", image)) => {
                let dim = |k: &[u8]| image.dict.get(k).ok().and_then(number);
                if let (Some(w), Some(h)) = (dim(b"Width"), dim(b"Height")) {
                    if let Some(dpi) = placement_dpi(ctm, w, h, self.user_unit) {
                        self.found.record(id, dpi);
                    }
                }
            }
            Some((b"Form", form)) if depth < MAX_DEPTH && !self.active.contains(&id) => {
                let m = form
                    .dict
                    .get(b"Matrix")
                    .ok()
                    .and_then(|o| resolve(doc, o))
                    .and_then(|o| o.as_array().ok())
                    .and_then(|a| matrix(a))
                    .unwrap_or(IDENTITY);
                let own = form
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|o| dict_of(doc, o));
                let resources = own.or(parent);
                if *self.draws_left == 0 {
                    self.unreadable(resources, depth + 1);
                    return;
                }
                *self.draws_left -= 1;
                let Some(data) = content_of(form, MAX_CONTENT) else {
                    self.unreadable(resources, depth + 1);
                    return;
                };
                self.active.push(id);
                if !self.walk(&data, resources, mul(&m, ctm), depth + 1) {
                    self.unreadable(resources, depth + 1);
                }
                self.active.pop();
            }
            _ => {}
        }
    }

    /// Marks every image `resources` lists as drawn somewhere unknown.
    fn unreadable(&mut self, resources: Option<&Dictionary>, depth: usize) {
        if let Some(r) = resources {
            let mut ids = Vec::new();
            images_listed(self.doc, r, depth, &mut HashSet::new(), &mut ids);
            self.found.unknown.extend(ids);
        }
    }
}

/// Walks every page.
pub(crate) fn placements(doc: &Document) -> Placements {
    let mut found = Placements::default();
    let mut draws_left = MAX_FORM_DRAWS;
    for page in doc.page_iter() {
        let resources = page_resources(doc, page);
        let user_unit = doc
            .get_dictionary(page)
            .ok()
            .and_then(|d| d.get(b"UserUnit").ok())
            .and_then(number)
            .filter(|u| *u > 0.0)
            .unwrap_or(1.0);
        let mut walker = Walker {
            doc,
            user_unit,
            found: &mut found,
            active: Vec::new(),
            draws_left: &mut draws_left,
        };
        let walked = page_content(doc, page, MAX_CONTENT)
            .is_some_and(|data| walker.walk(&data, resources, IDENTITY, 0));
        if !walked {
            walker.unreadable(resources, 0);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    /// A one-page document drawing `content`, with `xobjects` as its
    /// `/XObject` resources.
    fn doc_with(content: &str, xobjects: Vec<(&str, Object)>) -> (Document, Vec<ObjectId>) {
        let mut doc = Document::with_version("1.7");
        let mut ids = Vec::new();
        let mut xo = Dictionary::new();
        for (name, o) in xobjects {
            let id = doc.add_object(o);
            ids.push(id);
            xo.set(name, Object::Reference(id));
        }
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => xo },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        (doc, ids)
    }

    fn image(w: i64, h: i64) -> Object {
        Object::Stream(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w, "Height" => h,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
            },
            vec![0; (w * h) as usize],
        ))
    }

    fn form(content: &str, matrix: Option<[f32; 6]>, resources: Option<Dictionary>) -> Object {
        let mut dict = dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 1000.into(), 1000.into()],
        };
        if let Some(m) = matrix {
            dict.set(
                "Matrix",
                m.iter().map(|v| Object::Real(*v)).collect::<Vec<_>>(),
            );
        }
        if let Some(r) = resources {
            dict.set("Resources", r);
        }
        Object::Stream(Stream::new(dict, content.as_bytes().to_vec()))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn a_full_page_image_is_measured_on_its_less_dense_axis() {
        // 2550 x 3300 px on 8.5 x 11 in: 300 dpi both ways.
        let (doc, ids) = doc_with(
            "q 612 0 0 792 0 0 cm /Im0 Do Q",
            vec![("Im0", image(2550, 3300))],
        );
        let p = placements(&doc);
        assert!(close(p.dpi[&ids[0]], 300.0), "{:?}", p.dpi);
        // Stretched: 600 px across 4 in (150 dpi) and 600 px up 2 in (300).
        let (doc, ids) = doc_with("288 0 0 144 0 0 cm /Im0 Do", vec![("Im0", image(600, 600))]);
        assert!(close(placements(&doc).dpi[&ids[0]], 150.0));
    }

    #[test]
    fn q_and_q_restore_the_matrix_and_cm_concatenates() {
        // Scaled by 2 inside q..Q, then drawn at 72 x 72 pt outside it.
        let (doc, ids) = doc_with(
            "q 2 0 0 2 0 0 cm Q 72 0 0 72 0 0 cm /Im0 Do",
            vec![("Im0", image(300, 300))],
        );
        assert!(close(placements(&doc).dpi[&ids[0]], 300.0));
        // Two cm in a row multiply: 0.5 then 144 is 72 pt.
        let (doc, ids) = doc_with(
            "0.5 0 0 0.5 0 0 cm 144 0 0 144 0 0 cm /Im0 Do",
            vec![("Im0", image(300, 300))],
        );
        assert!(close(placements(&doc).dpi[&ids[0]], 300.0));
        // A rotated placement keeps its size.
        let (doc, ids) = doc_with(
            "0 72 -72 0 100 100 cm /Im0 Do",
            vec![("Im0", image(300, 300))],
        );
        assert!(close(placements(&doc).dpi[&ids[0]], 300.0));
        // A cm with the wrong number of operands is ignored.
        let (doc, ids) = doc_with(
            "72 0 0 72 0 0 cm 9 9 cm /Im0 Do",
            vec![("Im0", image(300, 300))],
        );
        assert!(close(placements(&doc).dpi[&ids[0]], 300.0));
    }

    #[test]
    fn an_image_drawn_twice_keeps_the_resolution_its_larger_drawing_needs() {
        let (doc, ids) = doc_with(
            "q 36 0 0 36 0 0 cm /Im0 Do Q q 288 0 0 288 0 0 cm /Im0 Do Q",
            vec![("Im0", image(600, 600))],
        );
        // 600 px over half an inch is 1200 dpi; over four inches, 150.
        assert!(close(placements(&doc).dpi[&ids[0]], 150.0));
    }

    #[test]
    fn a_placement_too_small_to_see_is_not_a_measurement() {
        let (doc, ids) = doc_with(
            "q 0 0 0 0 0 0 cm /Im0 Do Q q 288 0 0 288 0 0 cm /Im0 Do Q",
            vec![("Im0", image(600, 600))],
        );
        assert!(close(placements(&doc).dpi[&ids[0]], 150.0));
        let (doc, ids) = doc_with("0 0 0 0 0 0 cm /Im0 Do", vec![("Im0", image(600, 600))]);
        assert!(!placements(&doc).dpi.contains_key(&ids[0]));
    }

    #[test]
    fn forms_apply_their_matrix_and_resources() {
        let mut doc_and = doc_with("", vec![]);
        let doc = &mut doc_and.0;
        let img = doc.add_object(image(300, 300));
        let inner = form(
            "72 0 0 72 0 0 cm /Pic Do",
            Some([0.5, 0.0, 0.0, 0.5, 0.0, 0.0]),
            Some(dictionary! { "XObject" => dictionary! { "Pic" => img } }),
        );
        let inner = doc.add_object(inner);
        let page = doc.page_iter().next().unwrap();
        let content = doc.add_object(Stream::new(
            dictionary! {},
            b"2 0 0 2 0 0 cm /Fm0 Do".to_vec(),
        ));
        let page_dict = doc.get_object_mut(page).unwrap().as_dict_mut().unwrap();
        page_dict.set("Contents", content);
        page_dict.set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "Fm0" => inner } },
        );
        // 72 pt, halved by the form's matrix and doubled by the page: 72 pt.
        let p = placements(doc);
        assert!(close(p.dpi[&img], 300.0), "{:?}", p.dpi);
        assert_eq!(listed_images(doc), vec![img]);
    }

    #[test]
    fn a_form_without_resources_uses_its_parents_and_a_form_drawing_itself_stops() {
        let (mut doc, ids) = doc_with("", vec![("Im0", image(300, 300))]);
        let page = doc.page_iter().next().unwrap();
        let fm = doc.new_object_id();
        doc.objects
            .insert(fm, form("72 0 0 72 0 0 cm /Im0 Do /Fm0 Do", None, None));
        let content = doc.add_object(Stream::new(dictionary! {}, b"/Fm0 Do".to_vec()));
        let page_dict = doc.get_object_mut(page).unwrap().as_dict_mut().unwrap();
        page_dict.set("Contents", content);
        page_dict.set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "Fm0" => fm, "Im0" => ids[0] } },
        );
        let p = placements(&doc);
        assert!(close(p.dpi[&ids[0]], 300.0), "{:?}", p.dpi);
    }

    #[test]
    fn an_image_listed_but_never_drawn_is_listed_without_a_resolution() {
        let (doc, ids) = doc_with("BT ET", vec![("Im0", image(300, 300))]);
        assert_eq!(listed_images(&doc), ids);
        let p = placements(&doc);
        assert!(p.dpi.is_empty() && p.unknown.is_empty());
    }

    #[test]
    fn a_page_whose_content_cannot_be_read_marks_its_images_unknown() {
        let (mut doc, ids) = doc_with("", vec![("Im0", image(300, 300))]);
        let page = doc.page_iter().next().unwrap();
        // A filter lopdf cannot decode.
        let broken = doc.add_object(Stream::new(
            dictionary! { "Filter" => "JBIG2Decode" },
            b"not content".to_vec(),
        ));
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", broken);
        let p = placements(&doc);
        assert!(p.unknown.contains(&ids[0]));
    }

    #[test]
    fn content_with_a_bad_token_marks_its_images_unknown() {
        let (doc, ids) = doc_with(
            "q 10 0 0 10 0 0 cm /Im0 Do Q ) q 600 0 0 600 0 0 cm /Im0 Do Q",
            vec![("Im0", image(600, 600))],
        );
        let p = placements(&doc);
        assert!(p.unknown.contains(&ids[0]), "{:?}", p.dpi);
    }

    #[test]
    fn form_draws_past_the_budget_mark_their_images_unknown() {
        let (mut doc, ids) = doc_with("", vec![("Im0", image(300, 300))]);
        let fm = doc.add_object(form("72 0 0 72 0 0 cm /Im0 Do", None, None));
        let resources = dictionary! { "XObject" => dictionary! { "Fm0" => fm, "Im0" => ids[0] } };
        let mut found = Placements::default();
        let mut left = 3;
        let mut walker = Walker {
            doc: &doc,
            user_unit: 1.0,
            found: &mut found,
            active: Vec::new(),
            draws_left: &mut left,
        };
        assert!(walker.walk(b"/Fm0 Do /Fm0 Do /Fm0 Do", Some(&resources), IDENTITY, 0));
        assert!(walker.found.unknown.is_empty());
        walker.walk(b"/Fm0 Do", Some(&resources), IDENTITY, 0);
        assert!(found.unknown.contains(&ids[0]));
    }

    #[test]
    fn a_page_listing_one_stream_many_times_shares_one_limit() {
        let (mut doc, _) = doc_with("", vec![]);
        let page = doc.page_iter().next().unwrap();
        let part = doc.add_object(Stream::new(dictionary! {}, b"0 0 m".to_vec()));
        let parts: Vec<Object> = vec![part.into(); 10];
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", parts);
        assert_eq!(page_content(&doc, page, 60).map(|c| c.len()), Some(60));
        assert_eq!(page_content(&doc, page, 50), None);
    }

    #[test]
    fn user_unit_scales_the_page() {
        let (mut doc, ids) = doc_with("72 0 0 72 0 0 cm /Im0 Do", vec![("Im0", image(300, 300))]);
        let page = doc.page_iter().next().unwrap();
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("UserUnit", 2);
        assert!(close(placements(&doc).dpi[&ids[0]], 150.0));
    }

    #[test]
    fn resources_are_inherited_from_the_page_tree() {
        let (mut doc, ids) = doc_with("72 0 0 72 0 0 cm /Im0 Do", vec![("Im0", image(300, 300))]);
        let page = doc.page_iter().next().unwrap();
        let resources = doc
            .get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .remove(b"Resources")
            .unwrap();
        let parent = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        doc.get_object_mut(parent)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Resources", resources);
        assert!(close(placements(&doc).dpi[&ids[0]], 300.0));
    }
}
