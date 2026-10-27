//! The macOS clipboard (NSPasteboard): files, pictures and text.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::AnyThread;
use objc2_app_kit::{NSImage, NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{ns_string, NSArray, NSData, NSString, NSURL};

use super::{best_picture, files_in_text, path_of_uri, Got, Put, PICTURES};

const FILE_URL: &str = "public.file-url";
const TEXT: &str = "public.utf8-plain-text";
const HTML: &str = "public.html";
const TIFF: &str = "public.tiff";

fn file_url(path: &std::path::Path) -> Retained<NSString> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    url.absoluteString()
        .unwrap_or_else(|| NSString::from_str(""))
}

pub fn put(put: &Put) -> Result<(), String> {
    objc2::rc::autoreleasepool(|_| {
        let mut items: Vec<Retained<NSPasteboardItem>> = Vec::new();
        match put {
            Put::Text { text, html, .. } => {
                let item = NSPasteboardItem::new();
                let text = NSString::from_str(text);
                item.setString_forType(&text, &NSString::from_str(TEXT));
                if *html {
                    item.setString_forType(&text, &NSString::from_str(HTML));
                }
                items.push(item);
            }
            Put::Picture { file, format } => {
                let bytes = std::fs::read(file)
                    .map_err(|e| format!("couldn't read {}: {e}", file.display()))?;
                let item = NSPasteboardItem::new();
                item.setString_forType(&file_url(file), &NSString::from_str(FILE_URL));
                let native = PICTURES
                    .iter()
                    .find(|p| p.0 == *format)
                    .map(|p| p.2)
                    .filter(|uti| {
                        ["public.png", "public.jpeg", "com.compuserve.gif"].contains(uti)
                    });
                match native {
                    // Pasted as is wherever these are read, which is
                    // nearly everywhere.
                    Some(uti) => {
                        item.setData_forType(&NSData::with_bytes(&bytes), &NSString::from_str(uti));
                    }
                    // Few apps read webp, heic or the rest off the
                    // clipboard; TIFF is what every Mac app does.
                    None => {
                        let data = NSData::with_bytes(&bytes);
                        let tiff = NSImage::initWithData(NSImage::alloc(), &data)
                            .and_then(|img| img.TIFFRepresentation());
                        if let Some(tiff) = tiff {
                            item.setData_forType(&tiff, &NSString::from_str(TIFF));
                        }
                    }
                }
                items.push(item);
            }
            Put::Files(files) => {
                for file in files {
                    let item = NSPasteboardItem::new();
                    item.setString_forType(&file_url(file), &NSString::from_str(FILE_URL));
                    items.push(item);
                }
            }
        }
        let objects: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = items
            .into_iter()
            .map(ProtocolObject::from_retained)
            .collect();
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        if pb.writeObjects(&NSArray::from_retained_slice(&objects)) {
            Ok(())
        } else {
            Err("the clipboard didn't take it".to_string())
        }
    })
}

pub fn get() -> Result<Got, String> {
    objc2::rc::autoreleasepool(|_| {
        let pb = NSPasteboard::generalPasteboard();
        let items = pb.pasteboardItems().map(|a| a.to_vec()).unwrap_or_default();
        // Files copied in Finder: each item carries one.
        let files: Vec<PathBuf> = items
            .iter()
            .filter_map(|item| item.stringForType(ns_string!("public.file-url")))
            .filter_map(|url| path_of_uri(&url.to_string()))
            .collect();
        if !files.is_empty() {
            return Ok(Got::Files(files));
        }
        let types: Vec<String> = pb
            .types()
            .map(|t| t.to_vec().iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        if let Some((format, uti)) = best_picture(&types, |p| p.2) {
            if let Some(data) = pb.dataForType(&NSString::from_str(uti)) {
                return Ok(Got::Picture {
                    format,
                    bytes: data.to_vec(),
                });
            }
        }
        if let Some(text) = pb.stringForType(&NSString::from_str(TEXT)) {
            let text = text.to_string();
            return Ok(match files_in_text(&text) {
                Some(files) => Got::Files(files),
                None => Got::Text(text),
            });
        }
        Err("the clipboard is empty".to_string())
    })
}
