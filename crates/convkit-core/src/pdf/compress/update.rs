//! The qpdf JSON that puts new images in. `qpdf --update-from-json`
//! replaces each changed image stream by object id, taking its data from a
//! file as it is, and leaves every other object exactly as qpdf read it:
//! conv never writes the rest of the PDF itself.

use std::path::Path;

use lopdf::{Dictionary, Object, ObjectId};
use serde_json::{Map, Number, Value};

/// A byte PDF allows in a name as it is.
fn regular(b: u8) -> bool {
    (0x21..=0x7E).contains(&b) && !b"()<>[]{}/%#".contains(&b)
}

/// A name as qpdf's JSON spells it: `/Name`, or `n:/` and PDF's `#xx`
/// escapes when it holds anything else.
fn name(bytes: &[u8]) -> String {
    if bytes.iter().all(|&b| regular(b)) {
        return format!("/{}", String::from_utf8_lossy(bytes));
    }
    let mut s = String::from("n:/");
    for &b in bytes {
        if regular(b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("#{b:02x}"));
        }
    }
    s
}

/// A PDF object as qpdf's JSON writes it; `None` for what cannot appear in
/// a dictionary (a stream) or cannot be written (a real that is not finite).
fn value(o: &Object) -> Option<Value> {
    Some(match o {
        Object::Null => Value::Null,
        Object::Boolean(b) => Value::Bool(*b),
        Object::Integer(i) => Value::from(*i),
        // lopdf holds reals as f32; its shortest spelling is the number the
        // file had, to the precision lopdf kept.
        Object::Real(r) => Value::Number(Number::from_f64(format!("{r}").parse().ok()?)?),
        Object::Name(n) => Value::String(name(n)),
        Object::String(bytes, _) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            Value::String(format!("b:{hex}"))
        }
        Object::Array(a) => Value::Array(a.iter().map(value).collect::<Option<_>>()?),
        Object::Dictionary(d) => dictionary(d)?,
        Object::Reference((n, g)) => Value::String(format!("{n} {g} R")),
        Object::Stream(_) => return None,
    })
}

fn dictionary(d: &Dictionary) -> Option<Value> {
    let mut map = Map::new();
    for (k, v) in d.iter() {
        map.insert(name(k), value(v)?);
    }
    Some(Value::Object(map))
}

/// What a replaced stream's dictionary becomes: `dict` with `set` applied
/// and `/Length` and `/DecodeParms` dropped (qpdf writes the length, and
/// the new data has no parameters).
pub(crate) fn new_dict(dict: &Dictionary, set: &[(&str, Object)]) -> Dictionary {
    let mut d = dict.clone();
    d.remove(b"Length");
    d.remove(b"DecodeParms");
    for (k, v) in set {
        d.set(*k, v.clone());
    }
    d
}

/// One stream's entry: its new dictionary, and the file its data is in.
pub(crate) fn stream_entry(dict: &Dictionary, datafile: &Path) -> Option<Value> {
    let mut stream = Map::new();
    stream.insert("dict".into(), dictionary(dict)?);
    stream.insert(
        "datafile".into(),
        Value::String(datafile.to_string_lossy().into_owned()),
    );
    let mut entry = Map::new();
    entry.insert("stream".into(), Value::Object(stream));
    Some(Value::Object(entry))
}

/// The trailer, with a `/Size` when it has none. qpdf 12 repairs a file
/// whose trailer is lost by building one from the catalog it finds, but
/// without `/Size`, and every file it then writes from that one is broken
/// the same way. Given any `/Size`, qpdf writes the right one.
pub(crate) fn trailer_with_size(trailer: &Dictionary) -> Option<Value> {
    if trailer.get(b"Size").is_ok() {
        return None;
    }
    let mut t = trailer.clone();
    t.set("Size", 1);
    let mut entry = Map::new();
    entry.insert("value".into(), dictionary(&t)?);
    Some(Value::Object(entry))
}

/// The whole update document.
pub(crate) fn document(entries: Vec<(ObjectId, Value)>, trailer: Option<Value>) -> Value {
    let mut objects = Map::new();
    for ((n, g), entry) in entries {
        objects.insert(format!("obj:{n} {g} R"), entry);
    }
    if let Some(t) = trailer {
        objects.insert("trailer".into(), t);
    }
    serde_json::json!({ "qpdf": [{ "jsonversion": 2 }, objects] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, StringFormat};

    #[test]
    fn values_are_written_the_way_qpdf_reads_them() {
        let d = dictionary! {
            "Type" => "XObject",
            "Width" => 640,
            "Interpolate" => true,
            "Decode" => vec![Object::Real(1.0), Object::Real(0.25)],
            "SMask" => Object::Reference((12, 0)),
            "Name" => Object::Name(b"Im 1#".to_vec()),
            "ID" => Object::String(b"a\x01".to_vec(), StringFormat::Literal),
            "Nothing" => Object::Null,
            "Parms" => dictionary! { "K" => -1 },
        };
        let v = dictionary(&d).unwrap();
        assert_eq!(v["/Type"], "/XObject");
        assert_eq!(v["/Width"], 640);
        assert_eq!(v["/Interpolate"], true);
        assert_eq!(v["/Decode"], serde_json::json!([1.0, 0.25]));
        assert_eq!(v["/SMask"], "12 0 R");
        assert_eq!(v["/Name"], "n:/Im#201#23");
        assert_eq!(v["/ID"], "b:6101");
        assert_eq!(v["/Nothing"], Value::Null);
        assert_eq!(v["/Parms"]["/K"], -1);
    }

    #[test]
    fn a_trailer_without_a_size_gets_one() {
        let t = dictionary! { "Root" => Object::Reference((1, 0)) };
        let v = trailer_with_size(&t).unwrap();
        assert_eq!(v["value"]["/Root"], "1 0 R");
        assert_eq!(v["value"]["/Size"], 1);
        let doc = document(Vec::new(), Some(v));
        assert_eq!(doc["qpdf"][1]["trailer"]["value"]["/Size"], 1);
        let sized = dictionary! { "Root" => Object::Reference((1, 0)), "Size" => 9 };
        assert_eq!(trailer_with_size(&sized), None);
    }

    #[test]
    fn a_real_keeps_its_short_spelling() {
        assert_eq!(value(&Object::Real(0.1)).unwrap().to_string(), "0.1");
        assert_eq!(value(&Object::Real(f32::NAN)), None);
    }

    #[test]
    fn a_replaced_dictionary_loses_its_length_and_parameters() {
        let d = dictionary! {
            "Width" => 10, "Length" => 99, "DecodeParms" => dictionary! { "Predictor" => 15 },
            "Filter" => "FlateDecode",
        };
        let n = new_dict(&d, &[("Width", 5.into()), ("Filter", "DCTDecode".into())]);
        assert_eq!(n.get(b"Width").unwrap().as_i64().unwrap(), 5);
        assert_eq!(n.get(b"Filter").unwrap().as_name().unwrap(), b"DCTDecode");
        assert!(n.get(b"Length").is_err() && n.get(b"DecodeParms").is_err());
    }

    #[test]
    fn the_document_names_each_object() {
        let d = dictionary! { "Width" => 1 };
        let entry = stream_entry(&d, Path::new("/tmp/i.jpg")).unwrap();
        let doc = document(vec![((7, 0), entry)], None);
        assert_eq!(doc["qpdf"][0]["jsonversion"], 2);
        assert!(doc["qpdf"][1].get("trailer").is_none());
        assert_eq!(
            doc["qpdf"][1]["obj:7 0 R"]["stream"]["datafile"],
            "/tmp/i.jpg"
        );
        assert_eq!(doc["qpdf"][1]["obj:7 0 R"]["stream"]["dict"]["/Width"], 1);
    }
}
