//! `--compress` against a real qpdf, on PDFs built here in memory.
//!
//! `#[ignore]`-gated like `pdf.rs`: `cargo test` stays green without qpdf,
//! and `cargo test -- --ignored` runs these wherever qpdf resolves (PATH,
//! `conv install qpdf`, or `CONVKIT_QPDF`).

use std::path::{Path, PathBuf};
use std::process::Command;

use convkit_core::exec::{self, Request};
use convkit_core::sized::Strategy;
use convkit_core::{Backend, ErrorCode, Format, Outcome, Resolver, Tuning};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

fn qpdf() -> PathBuf {
    Resolver::new()
        .resolve(Backend::Qpdf)
        .unwrap_or_else(|e| panic!("backend_missing: qpdf not found -- {}", e.message))
        .path
}

fn qpdf_ok(args: &[&str]) -> String {
    let out = Command::new(qpdf()).args(args).output().unwrap();
    assert!(
        matches!(out.status.code(), Some(0) | Some(3)),
        "qpdf {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn json(p: &Path, keys: &[&str]) -> serde_json::Value {
    let mut args: Vec<String> = vec!["--json".into()];
    args.extend(keys.iter().map(|k| format!("--json-key={k}")));
    args.push(s(p).into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    serde_json::from_str(&qpdf_ok(&args)).unwrap()
}

/// Every image the pages draw, once each: (object, width, height, filter).
fn images(p: &Path) -> Vec<(String, i64, i64, String)> {
    let v = json(p, &["pages"]);
    let mut out: Vec<(String, i64, i64, String)> = Vec::new();
    for page in v["pages"].as_array().unwrap() {
        for im in page["images"].as_array().unwrap() {
            let obj = im["object"].as_str().unwrap().to_string();
            if out.iter().any(|o| o.0 == obj) {
                continue;
            }
            let filter = im["filter"]
                .as_array()
                .and_then(|f| f.last())
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string();
            out.push((
                obj,
                im["width"].as_i64().unwrap(),
                im["height"].as_i64().unwrap(),
                filter,
            ));
        }
    }
    out
}

fn compress(input: &Path, output: &Path, allow: bool) -> convkit_core::Result<Outcome> {
    let req = Request {
        from: Format::Pdf,
        to: Format::Pdf,
        inputs: vec![input.to_path_buf()],
        output: output.to_path_buf(),
        overwrite: false,
        tuning: Tuning {
            compress: true,
            ..Tuning::default()
        },
        allow_extreme: allow,
    };
    exec::run(&req, &Resolver::new(), &mut |_| {})
}

fn sized(
    input: &Path,
    output: &Path,
    size: &str,
    compress: bool,
    allow: bool,
) -> convkit_core::Result<Outcome> {
    let req = Request {
        from: Format::Pdf,
        to: Format::Pdf,
        inputs: vec![input.to_path_buf()],
        output: output.to_path_buf(),
        overwrite: true,
        tuning: Tuning {
            compress,
            max_size: Some(convkit_core::size::parse(size).unwrap()),
            ..Tuning::default()
        },
        allow_extreme: allow,
    };
    exec::run(&req, &Resolver::new(), &mut |_| {})
}

fn checks_clean(p: &Path) {
    let out = qpdf_ok(&["--check", s(p)]);
    assert!(out.contains("No syntax or stream encoding errors"), "{out}");
}

// --- Building PDFs --------------------------------------------------------

/// Photo-like samples: gradients, a soft blob and a little grain.
fn photo(width: u32, height: u32, channels: usize, seed: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height) as usize * channels);
    let mut r = seed;
    for y in 0..height {
        for x in 0..width {
            let fx = f64::from(x) / f64::from(width);
            let fy = f64::from(y) / f64::from(height);
            let blob = (-((fx - 0.5).powi(2) + (fy - 0.4).powi(2)) * 8.0).exp();
            for c in 0..channels {
                r = r.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let grain = f64::from((r >> 16) % 9) - 4.0;
                let v =
                    30.0 + 120.0 * fx * (c as f64 * 0.3 + 0.5) + 80.0 * blob + 20.0 * fy + grain;
                data.push(v.clamp(0.0, 255.0) as u8);
            }
        }
    }
    data
}

/// Flat colours and one-pixel strokes, as a screenshot of text.
fn screenshot(width: u32, height: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let stroke = (y % 12 < 9) && (x % 7 == 0 || (y % 12 == 4 && x % 3 != 0));
            let v = if stroke { 20 } else { 245 };
            data.extend([v, v, if stroke { 20 } else { 250 }]);
        }
    }
    data
}

fn jpeg(
    data: &[u8],
    width: u32,
    height: u32,
    color: jpeg_encoder::ColorType,
    quality: u8,
) -> Vec<u8> {
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(data, width as u16, height as u16, color)
        .unwrap();
    out
}

fn flate(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn image(
    doc: &mut Document,
    width: u32,
    height: u32,
    cs: &str,
    filter: &str,
    data: Vec<u8>,
) -> ObjectId {
    doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => width as i64, "Height" => height as i64,
            "ColorSpace" => cs, "BitsPerComponent" => 8, "Filter" => filter,
        },
        data,
    ))
}

/// A document of US Letter pages, each drawing `content` with `resources`;
/// `shared` puts one resource dictionary on the page tree for every page.
struct Builder {
    doc: Document,
    pages_id: ObjectId,
    kids: Vec<Object>,
}

impl Builder {
    fn new() -> Self {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        Builder {
            doc,
            pages_id,
            kids: Vec::new(),
        }
    }

    fn page(
        &mut self,
        content: &str,
        resources: Option<Dictionary>,
        extra: Dictionary,
    ) -> ObjectId {
        let content_id = self
            .doc
            .add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => self.pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
        };
        if let Some(r) = resources {
            page.set("Resources", r);
        }
        for (k, v) in extra.into_iter() {
            page.set(k, v);
        }
        let id = self.doc.add_object(page);
        self.kids.push(id.into());
        id
    }

    fn write(mut self, path: &Path, tree_resources: Option<Dictionary>, catalog: Dictionary) {
        let mut pages = dictionary! {
            "Type" => "Pages",
            "Kids" => self.kids.clone(),
            "Count" => self.kids.len() as i64,
        };
        if let Some(r) = tree_resources {
            pages.set("Resources", r);
        }
        self.doc
            .objects
            .insert(self.pages_id, Object::Dictionary(pages));
        let mut cat = dictionary! { "Type" => "Catalog", "Pages" => self.pages_id };
        for (k, v) in catalog.into_iter() {
            cat.set(k, v);
        }
        let cat_id = self.doc.add_object(cat);
        self.doc.trailer.set("Root", cat_id);
        self.doc.save(path).unwrap();
    }
}

fn xobjects(entries: &[(&str, ObjectId)]) -> Dictionary {
    let mut xo = Dictionary::new();
    for (n, id) in entries {
        xo.set(*n, *id);
    }
    dictionary! { "XObject" => xo }
}

fn full_page(name: &str) -> String {
    format!("q 612 0 0 792 0 0 cm /{name} Do Q")
}

/// A letter page holding one scanned photo at `dpi`.
fn scan(path: &Path, dpi: u32, quality: u8, gray: bool) {
    let (w, h) = (dpi * 17 / 2, dpi * 11);
    let mut b = Builder::new();
    let (channels, color, cs) = if gray {
        (1, jpeg_encoder::ColorType::Luma, "DeviceGray")
    } else {
        (3, jpeg_encoder::ColorType::Rgb, "DeviceRGB")
    };
    let data = jpeg(&photo(w, h, channels, 7), w, h, color, quality);
    let im = image(&mut b.doc, w, h, cs, "DCTDecode", data);
    b.page(
        &full_page("Im0"),
        Some(xobjects(&[("Im0", im)])),
        dictionary! {},
    );
    b.write(path, None, dictionary! {});
}

/// A page of Helvetica text.
fn text_page(b: &mut Builder, lines: usize, extra: Dictionary) -> ObjectId {
    let mut content = String::from("BT /F1 11 Tf 72 740 Td 14 TL\n");
    for i in 0..lines {
        content.push_str(&format!(
            "(Line {i}: the quick brown fox jumps over the lazy dog.) '\n"
        ));
    }
    content.push_str("ET");
    let font = dictionary! { "F1" => dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    } };
    b.page(&content, Some(dictionary! { "Font" => font }), extra)
}

// --- Tests ----------------------------------------------------------------

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_300_dpi_scan_is_downsampled_to_150_dpi() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("scan.pdf");
    scan(&input, 300, 90, false);
    let out = dir.path().join("scan-compressed.pdf");

    let o = compress(&input, &out, false).unwrap();

    checks_clean(&out);
    let im = images(&out);
    assert_eq!(im.len(), 1);
    assert_eq!(
        (im[0].1, im[0].2, im[0].3.as_str()),
        (1275, 1650, "/DCTDecode")
    );
    let sz = o.sizing.unwrap();
    assert_eq!(sz.strategy, Strategy::Encode);
    let pdf = sz.pdf.unwrap();
    assert_eq!((pdf.level, pdf.images_downsampled), (Some(1), 1));
    assert!(
        o.bytes * 2 < pdf.source_bytes,
        "{} of {}",
        o.bytes,
        pdf.source_bytes
    );
    assert_eq!(o.warnings, vec!["Downsampled 1 image to 150 dpi."]);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_png_photo_becomes_a_jpeg_and_a_png_screenshot_stays_lossless() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("deck.pdf");
    let mut b = Builder::new();
    let ph = image(
        &mut b.doc,
        800,
        600,
        "DeviceRGB",
        "FlateDecode",
        flate(&photo(800, 600, 3, 3)),
    );
    let sc = image(
        &mut b.doc,
        800,
        600,
        "DeviceRGB",
        "FlateDecode",
        flate(&screenshot(800, 600)),
    );
    // Each drawn 6 in wide: 133 dpi, under the threshold.
    let content = "q 432 0 0 324 90 420 cm /Ph Do Q q 432 0 0 324 90 40 cm /Sc Do Q";
    b.page(
        content,
        Some(xobjects(&[("Ph", ph), ("Sc", sc)])),
        dictionary! {},
    );
    b.write(&input, None, dictionary! {});
    let out = dir.path().join("out.pdf");

    let o = compress(&input, &out, false).unwrap();

    let im = images(&out);
    let filters: Vec<(i64, &str)> = im.iter().map(|i| (i.1, i.3.as_str())).collect();
    assert!(filters.contains(&(800, "/DCTDecode")), "{im:?}");
    assert!(filters.contains(&(800, "/FlateDecode")), "{im:?}");
    assert_eq!(o.sizing.unwrap().pdf.unwrap().images_reencoded, 1);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn pages_sharing_one_resource_dictionary_are_rewritten_once() {
    // qpdf --optimize-images wrote one copy per page of every image here.
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("deck.pdf");
    let mut b = Builder::new();
    let mut names = Vec::new();
    for i in 0..4 {
        let data = jpeg(
            &photo(900, 600, 3, i),
            900,
            600,
            jpeg_encoder::ColorType::Rgb,
            95,
        );
        names.push((
            format!("Im{i}"),
            image(&mut b.doc, 900, 600, "DeviceRGB", "DCTDecode", data),
        ));
    }
    let entries: Vec<(&str, ObjectId)> = names.iter().map(|(n, id)| (n.as_str(), *id)).collect();
    let shared = xobjects(&entries);
    for p in 0..6 {
        let n = &names[p % 4].0;
        b.page(
            &format!("q 216 0 0 144 72 72 cm /{n} Do Q"),
            None,
            dictionary! {},
        );
    }
    b.write(&input, Some(shared), dictionary! {});
    let before = images(&input);
    let out = dir.path().join("out.pdf");

    let o = compress(&input, &out, false).unwrap();

    let after = images(&out);
    assert_eq!(after.len(), before.len(), "{after:?}");
    assert!(after.iter().all(|i| i.1 < 900), "downsampled: {after:?}");
    assert!(o.bytes < std::fs::metadata(&input).unwrap().len());
    assert_eq!(o.sizing.unwrap().pdf.unwrap().images_downsampled, 4);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn form_fields_tags_links_and_bookmarks_survive() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("form.pdf");
    let mut b = Builder::new();
    let field_ids: Vec<ObjectId> = (0..3).map(|_| b.doc.new_object_id()).collect();
    let link = b.doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Link",
        "Rect" => vec![72.into(), 700.into(), 200.into(), 712.into()],
        "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com") },
    });
    let mut annots: Vec<Object> = field_ids.iter().map(|&id| id.into()).collect();
    annots.push(link.into());
    let ph = jpeg(
        &photo(2550, 1650, 3, 9),
        2550,
        1650,
        jpeg_encoder::ColorType::Rgb,
        90,
    );
    let ph = image(&mut b.doc, 2550, 1650, "DeviceRGB", "DCTDecode", ph);
    let page = b.page(
        "q 612 0 0 396 0 0 cm /Ph Do Q",
        Some(xobjects(&[("Ph", ph)])),
        dictionary! { "Annots" => annots, "StructParents" => 0 },
    );
    for (i, &id) in field_ids.iter().enumerate() {
        let y = 600 - 30 * i as i64;
        b.doc.objects.insert(
            id,
            Object::Dictionary(dictionary! {
                "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx",
                "T" => Object::string_literal(format!("field{i}")),
                "V" => Object::string_literal(format!("value {i}")),
                "Rect" => vec![72.into(), y.into(), 300.into(), (y + 20).into()],
                "P" => page,
            }),
        );
    }
    let fields: Vec<Object> = field_ids.iter().map(|&id| id.into()).collect();
    let acroform = b.doc.add_object(dictionary! { "Fields" => fields });
    let outline_id = b.doc.new_object_id();
    let first = b.doc.add_object(dictionary! {
        "Title" => Object::string_literal("One"), "Parent" => outline_id,
        "Dest" => vec![page.into(), "Fit".into()],
    });
    b.doc.objects.insert(
        outline_id,
        Object::Dictionary(
            dictionary! { "Type" => "Outlines", "First" => first, "Last" => first, "Count" => 1 },
        ),
    );
    let struct_root = b.doc.add_object(dictionary! { "Type" => "StructTreeRoot" });
    b.write(
        &input,
        None,
        dictionary! {
            "AcroForm" => acroform, "Outlines" => outline_id,
            "MarkInfo" => dictionary! { "Marked" => true }, "StructTreeRoot" => struct_root,
        },
    );
    let out = dir.path().join("out.pdf");

    let o = compress(&input, &out, false).unwrap();
    assert_eq!(o.sizing.unwrap().pdf.unwrap().images_downsampled, 1);

    let keys = ["acroform", "outlines", "pages"];
    let (a, z) = (json(&input, &keys), json(&out, &keys));
    let values = |v: &serde_json::Value| -> Vec<String> {
        v["acroform"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["value"].as_str().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(values(&z), values(&a));
    assert_eq!(values(&z).len(), 3);
    assert_eq!(z["outlines"].as_array().unwrap().len(), 1);
    let cat = json(&out, &["qpdf"]);
    let objects = cat["qpdf"][1].as_object().unwrap();
    let root = objects
        .values()
        .find(|o| o["value"]["/Type"] == "/Catalog")
        .unwrap();
    assert!(root["value"].get("/StructTreeRoot").is_some());
    let page_obj = z["pages"][0]["object"].as_str().unwrap();
    let annots = &objects[&format!("obj:{page_obj}")]["value"]["/Annots"];
    assert_eq!(
        annots.as_array().unwrap().len(),
        4,
        "three widgets and the link"
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_signed_pdf_needs_a_yes_and_says_the_signature_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("signed.pdf");
    let mut b = Builder::new();
    let ph = jpeg(
        &photo(2550, 3300, 1, 2),
        2550,
        3300,
        jpeg_encoder::ColorType::Luma,
        90,
    );
    let ph = image(&mut b.doc, 2550, 3300, "DeviceGray", "DCTDecode", ph);
    b.page(
        &full_page("Ph"),
        Some(xobjects(&[("Ph", ph)])),
        dictionary! {},
    );
    let sig = b.doc.add_object(dictionary! {
        "Type" => "Sig", "Filter" => "Adobe.PPKLite", "SubFilter" => "adbe.pkcs7.detached",
        "Contents" => Object::String(vec![0; 64], lopdf::StringFormat::Hexadecimal),
        "ByteRange" => vec![0.into(), 10.into(), 20.into(), 10.into()],
    });
    let field = b.doc.add_object(dictionary! {
        "FT" => "Sig", "T" => Object::string_literal("Signature1"), "V" => sig,
        "Rect" => vec![0.into(), 0.into(), 0.into(), 0.into()],
    });
    let form = b
        .doc
        .add_object(dictionary! { "Fields" => vec![field.into()], "SigFlags" => 3 });
    b.write(&input, None, dictionary! { "AcroForm" => form });
    let out = dir.path().join("out.pdf");

    let e = compress(&input, &out, false).unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert!(e.message.contains("digitally signed"), "{}", e.message);
    assert!(!out.exists());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
    assert_eq!(leftovers.len(), 1, "only the input is left");

    let o = compress(&input, &out, true).unwrap();
    assert!(
        o.notes.iter().any(|n| n.contains("not validly signed")),
        "{:?}",
        o.notes
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn an_owner_password_pdf_keeps_its_restrictions() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.pdf");
    scan(&plain, 300, 90, true);
    let input = dir.path().join("locked.pdf");
    qpdf_ok(&[
        "--encrypt",
        "",
        "owner-secret",
        "256",
        "--print=none",
        "--modify=none",
        "--",
        s(&plain),
        s(&input),
    ]);
    let out = dir.path().join("out.pdf");

    let o = compress(&input, &out, false).unwrap();

    assert_eq!(o.sizing.unwrap().strategy, Strategy::Encode);
    let enc = qpdf_ok(&["--show-encryption", s(&out)]);
    assert!(enc.contains("print low resolution: not allowed"), "{enc}");
    assert!(enc.contains("modify other: not allowed"), "{enc}");
    assert_eq!(images(&out)[0].1, 1275);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_text_only_pdf_never_grows() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("text.pdf");
    let mut b = Builder::new();
    for _ in 0..3 {
        text_page(&mut b, 40, dictionary! {});
    }
    b.write(&input, None, dictionary! {});
    // Already as qpdf would write it, so its rewrite cannot be smaller.
    let tight = dir.path().join("tight.pdf");
    qpdf_ok(&[
        s(&input),
        "--object-streams=generate",
        "--recompress-flate",
        "--compression-level=9",
        s(&tight),
    ]);
    let out = dir.path().join("out.pdf");

    let o = compress(&tight, &out, false).unwrap();

    assert!(o.bytes <= std::fs::metadata(&tight).unwrap().len());
    if o.sizing.as_ref().unwrap().strategy == Strategy::Copy {
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&tight).unwrap());
        assert!(
            o.warnings[0].starts_with("Already compact: this PDF is mostly text"),
            "{:?}",
            o.warnings
        );
    }
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn cmyk_stays_cmyk_with_its_decode_array_and_its_values() {
    let dir = tempfile::tempdir().unwrap();
    for with_decode in [false, true] {
        let input = dir.path().join(format!("cmyk-{with_decode}.pdf"));
        let mut b = Builder::new();
        let (w, h) = (1600u32, 1200u32);
        let ink = photo(w, h, 4, 5);
        let data = jpeg(&ink, w, h, jpeg_encoder::ColorType::Cmyk, 92);
        let im = image(&mut b.doc, w, h, "DeviceCMYK", "DCTDecode", data.clone());
        if with_decode {
            let decode: Vec<Object> = (0..8).map(|i| Object::Integer(1 - i % 2)).collect();
            b.doc
                .get_object_mut(im)
                .unwrap()
                .as_stream_mut()
                .unwrap()
                .dict
                .set("Decode", decode);
        }
        // 4 in wide: 400 dpi.
        b.page(
            "q 288 0 0 216 72 72 cm /Im0 Do Q",
            Some(xobjects(&[("Im0", im)])),
            dictionary! {},
        );
        b.write(&input, None, dictionary! {});
        let out = dir.path().join(format!("out-{with_decode}.pdf"));

        compress(&input, &out, false).unwrap();

        let doc = Document::load(&out).unwrap();
        let page = doc.page_iter().next().unwrap();
        let (res, ids) = doc.get_page_resources(page).unwrap();
        let xo = res
            .or_else(|| ids.first().and_then(|id| doc.get_dictionary(*id).ok()))
            .unwrap()
            .get(b"XObject")
            .unwrap();
        let xo = match xo {
            Object::Reference(id) => doc.get_dictionary(*id).unwrap(),
            Object::Dictionary(d) => d,
            _ => panic!(),
        };
        let id = xo.get(b"Im0").unwrap().as_reference().unwrap();
        let st = doc.get_object(id).unwrap().as_stream().unwrap();
        assert_eq!(
            st.dict.get(b"ColorSpace").unwrap().as_name().unwrap(),
            b"DeviceCMYK"
        );
        assert_eq!(st.dict.get(b"Width").unwrap().as_i64().unwrap(), 600);
        assert_eq!(st.dict.get(b"Decode").is_ok(), with_decode);
        // The values a decoder reads are the ones the source held.
        let mean = |jpeg: &[u8]| -> Vec<f64> {
            let mut d = jpeg_decoder::Decoder::new(jpeg);
            let px = d.decode().unwrap();
            (0..4)
                .map(|c| {
                    px.iter()
                        .skip(c)
                        .step_by(4)
                        .map(|&v| f64::from(v))
                        .sum::<f64>()
                        / (px.len() / 4) as f64
                })
                .collect()
        };
        let (a, z) = (mean(&data), mean(&st.content));
        for c in 0..4 {
            assert!((a[c] - z[c]).abs() < 2.0, "channel {c}: {a:?} vs {z:?}");
        }
    }
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_200_dpi_office_scan_suggests_150_dpi_with_its_size() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("office.pdf");
    scan(&input, 200, 75, true);
    let out = dir.path().join("out.pdf");

    let o = compress(&input, &out, false).unwrap();

    let sz = o.sizing.unwrap();
    let suggested = sz.suggested.clone().expect("a suggestion");
    let target = convkit_core::size::parse(&suggested).unwrap().bytes;
    let source = sz.pdf.as_ref().unwrap().source_bytes;
    assert!(
        target * 4 <= source * 3 + source / 10,
        "{target} of {source}"
    );
    let note = o.warnings.join(" ");
    assert!(note.contains("At 150 dpi it would be"), "{note}");
    assert!(
        note.contains(&format!("try --max-size {suggested}")),
        "{note}"
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_damaged_pdf_is_repaired_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.pdf");
    scan(&good, 300, 90, true);
    let plain = dir.path().join("plain.pdf");
    qpdf_ok(&["--object-streams=disable", s(&good), s(&plain)]);
    let mut bytes = std::fs::read(&plain).unwrap();
    bytes.truncate(bytes.len() - 40); // cut off the trailer and startxref
    let input = dir.path().join("damaged.pdf");
    std::fs::write(&input, bytes).unwrap();
    let out = dir.path().join("out.pdf");

    let o = compress(&input, &out, false).unwrap();

    checks_clean(&out);
    assert_eq!(
        o.notes,
        vec!["damaged.pdf was damaged and has been repaired; check out.pdf."]
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn max_size_copies_a_pdf_already_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("scan.pdf");
    scan(&input, 150, 75, true);
    let out = dir.path().join("out.pdf");

    let o = sized(&input, &out, "50mb", false, false).unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&input).unwrap());
    let sz = o.sizing.unwrap();
    assert_eq!(
        (sz.strategy, sz.attempts, sz.target_bytes),
        (Strategy::Copy, 0, Some(50_000_000))
    );
    assert!(
        o.warnings[0].contains("under 50 MB; copied as it is"),
        "{:?}",
        o.warnings
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn max_size_climbs_to_the_first_level_under_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("office.pdf");
    scan(&input, 200, 75, true);
    let out = dir.path().join("out.pdf");
    // The size --compress suggests is what L2 reaches.
    let suggested = compress(&input, &dir.path().join("c.pdf"), false)
        .unwrap()
        .sizing
        .unwrap()
        .suggested
        .unwrap();

    let o = sized(&input, &out, &suggested, false, false).unwrap();

    checks_clean(&out);
    let sz = o.sizing.unwrap();
    let pdf = sz.pdf.unwrap();
    assert_eq!(
        (pdf.level, sz.attempts, sz.over_target),
        (Some(2), 3, false)
    );
    assert!(o.bytes <= convkit_core::size::parse(&suggested).unwrap().bytes);
    assert!(
        o.warnings[0].starts_with("Sized with images at up to 150 dpi"),
        "{:?}",
        o.warnings
    );
    // From --compress, L0 is not tried.
    let o = sized(&input, &out, &suggested, true, false).unwrap();
    assert_eq!(o.sizing.unwrap().attempts, 2);
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn an_extreme_target_asks_first_and_names_a_readable_size() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("scan.pdf");
    scan(&input, 300, 90, false);
    let out = dir.path().join("out.pdf");

    let e = sized(&input, &out, "2kb", false, false).unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert!(
        e.message.contains("down to 96 dpi or less"),
        "{}",
        e.message
    );
    let fix = e.remediation.unwrap().manual.unwrap();
    assert!(
        fix.starts_with("pass --yes to make it anyway, or try --max-size "),
        "{fix}"
    );
    assert!(!out.exists());

    let o = sized(&input, &out, "2kb", false, true).unwrap();
    checks_clean(&out);
    let level = o
        .sizing
        .as_ref()
        .unwrap()
        .pdf
        .as_ref()
        .unwrap()
        .level
        .unwrap();
    assert!(level >= 4, "{level}");
    assert!(
        o.notes
            .iter()
            .any(|n| n.contains("small print may be hard to read")),
        "{:?}",
        o.notes
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_target_no_level_reaches_keeps_the_smallest_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("text.pdf");
    let mut b = Builder::new();
    for _ in 0..3 {
        text_page(&mut b, 40, dictionary! {});
    }
    b.write(&input, None, dictionary! {});
    let out = dir.path().join("out.pdf");

    let o = sized(&input, &out, "1kb", false, true).unwrap();

    let sz = o.sizing.unwrap();
    assert!(sz.over_target);
    assert_eq!(sz.attempts, 1, "no image to change, so one try");
    assert!(o.bytes <= std::fs::metadata(&input).unwrap().len());
    assert!(
        o.notes
            .iter()
            .any(|n| n.starts_with("Could not get under 1 KB")),
        "{:?}",
        o.notes
    );
}

#[test]
#[ignore = "requires qpdf; run with --ignored"]
fn a_pdf_that_needs_a_password_to_open_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.pdf");
    scan(&plain, 300, 90, true);
    let input = dir.path().join("locked.pdf");
    qpdf_ok(&[
        "--encrypt",
        "open-me",
        "owner-secret",
        "256",
        "--",
        s(&plain),
        s(&input),
    ]);
    let out = dir.path().join("out.pdf");

    let e = compress(&input, &out, true).unwrap_err();

    assert_eq!(e.code, ErrorCode::PasswordProtected);
    assert_eq!(e.message, "locked.pdf is password-protected");
    let way = e.remediation.and_then(|r| r.manual).unwrap_or_default();
    assert_eq!(
        way,
        "unlock it first, `conv locked.pdf --unlock`, then `conv locked-unlocked.pdf --compress`"
    );
    assert!(!out.exists());
}
