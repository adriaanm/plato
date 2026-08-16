//! What a pushed article leaves behind: an EPUB in `inbox/`.
//!
//! A link sent with `platonic URL` opens in the article view immediately, but
//! the view's page lives in memory and the WiFi it arrived over is about to
//! go back to sleep. This files the same page -- body, images and all -- as a
//! self-contained EPUB under the library's `inbox/`, so the article is a
//! normal inbox item: it appears in the library, opens offline, and expires
//! under the same sweep as every other pushed document.
//!
//! EPUB rather than a bare `.html`, for one reason: the images. An HTML file
//! resolves its `img-0` srcs against its parent directory, which would spill
//! a dozen loose image files into `inbox/` for the sweep and the listing to
//! trip over; a zip keeps the whole article one file with one mtime.
//!
//! The mtime is the **Mac's** clock, carried here from the `open-url` FIFO
//! line. It is as much a part of the document as the bytes -- the inbox sweep
//! judges lifetimes against it, and the device's own clock reads 2023.

use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{format_err, Error};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::{escape_text, Page};
use crate::document::asciify;

/// Serialize a rendered article page as EPUB bytes.
///
/// The body goes in verbatim: it came out of `sanitize_article_fragment`, so
/// every element closes and the vocabulary is ours -- which is also what
/// makes it valid XHTML for the EPUB engine, the same engine that laid it
/// out in the article view. The images keep their in-memory names (`img-0`,
/// ...): MuPDF sniffs formats from the bytes, so the names need no
/// extensions, and the body's srcs need no rewriting.
pub fn epub(page: &Page, url: &str) -> Result<Vec<u8>, Error> {
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // `mimetype` first and stored, per the spec -- it is how anything
    // sniffing the container (`guess_kind` included) can find the mime type
    // at a fixed offset instead of parsing zip structures.
    zip.start_file("mimetype",
                   SimpleFileOptions::default()
                       .compression_method(CompressionMethod::Stored))?;
    zip.write_all(b"application/epub+zip")?;

    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated);

    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                    <container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
                    <rootfiles>\
                    <rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/>\
                    </rootfiles>\
                    </container>")?;

    let mut names: Vec<&String> = page.images.keys().collect();
    names.sort();

    let mut opf = String::new();
    opf.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                  <package xmlns=\"http://www.idpf.org/2007/opf\" \
                  unique-identifier=\"source\" version=\"2.0\">\
                  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">");
    opf.push_str(&format!("<dc:title>{}</dc:title>", escape_text(&page.title)));
    // The URL is the identifier: it is the one name the article had before
    // it was ours, and `escape_text` has already refused it any markup.
    opf.push_str(&format!("<dc:identifier id=\"source\">{}</dc:identifier>",
                          escape_text(url)));
    opf.push_str("</metadata><manifest>\
                  <item id=\"article\" href=\"article.xhtml\" media-type=\"application/xhtml+xml\"/>");
    for name in &names {
        opf.push_str(&format!("<item id=\"{name}\" href=\"{name}\" media-type=\"{}\"/>",
                              media_type(&page.images[name.as_str()])));
    }
    opf.push_str("</manifest><spine><itemref idref=\"article\"/></spine></package>");
    zip.start_file("OEBPS/content.opf", deflated)?;
    zip.write_all(opf.as_bytes())?;

    zip.start_file("OEBPS/article.xhtml", deflated)?;
    zip.write_all(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                           <html xmlns=\"http://www.w3.org/1999/xhtml\">\
                           <head><title>{}</title></head>\
                           <body>{}</body></html>",
                          escape_text(&page.title), page.body).as_bytes())?;

    for name in &names {
        zip.start_file(format!("OEBPS/{name}"), deflated)?;
        zip.write_all(&page.images[name.as_str()])?;
    }

    Ok(zip.finish()?.into_inner())
}

/// What the manifest calls an image, read off its first bytes. The engine
/// itself never looks -- MuPDF sniffs the bytes again at draw time -- so an
/// unrecognized format is declared honestly rather than guessed at.
fn media_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "application/octet-stream"
    }
}

/// An article title as an inbox filename: `asciify`d, lowercased, hyphens for
/// everything else, bounded -- well inside the receiver's own component rules,
/// so a saved article's name would survive any path a pushed document's does.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in asciify(title).chars() {
        if out.len() >= 64 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "article".to_string()
    } else {
        out
    }
}

/// Write the page into `<library>/inbox/` and stamp it, returning the path.
///
/// Two pushes of the same article are two files (`-2`, `-3`, ...): the second
/// push usually means the first expired or was deleted, and silently
/// overwriting a file the sweep is tracking would resurrect it with a new
/// clock.
pub fn save(page: &Page, url: &str, stamp: i64, library_home: &Path)
            -> Result<PathBuf, Error> {
    let dir = library_home.join("inbox");
    fs::create_dir_all(&dir)?;
    let base = slug(&page.title);
    let path = (1..=99)
        .map(|n| match n {
            1 => dir.join(format!("{base}.epub")),
            n => dir.join(format!("{base}-{n}.epub")),
        })
        .find(|p| !p.exists())
        .ok_or_else(|| format_err!("a hundred copies of {base:?} already"))?;

    let bytes = epub(page, url)?;
    let mut file = File::create(&path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    // Both stamps, and the atime NOT omitted: on the userstore's FUSE layer,
    // futimens() with UTIME_OMIT succeeds and silently changes nothing --
    // the same finding platonic-recv's PUT is built around (server.rs,
    // confirmed on the device 2026-08-12).
    let when = if stamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(stamp as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(stamp.unsigned_abs())
    };
    file.set_times(FileTimes::new().set_accessed(when).set_modified(when))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxhash::FxHashMap;
    use crate::document::epub::EpubDocument;
    use crate::document::Document;

    fn page_with_a_picture() -> Page {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 1, 1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0]).unwrap();
        drop(writer);

        let mut images = FxHashMap::default();
        images.insert("img-0".to_string(), out);
        Page {
            title: "Benedetta's Ragú — a title & test".to_string(),
            body: "<div class=\"head\"><h1>Benedetta's Ragú</h1></div>\
                   <div class=\"article\"><p>Brown the pork.</p>\
                   <img src=\"img-0\"/></div>".to_string(),
            images,
        }
    }

    #[test]
    fn the_saved_epub_opens_reads_and_keeps_its_stamp() {
        let dir = std::env::temp_dir().join(format!(
            "plato-save-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(UNIX_EPOCH)
                .unwrap().as_nanos()));
        fs::create_dir_all(&dir).unwrap();

        let page = page_with_a_picture();
        let stamp = 1_700_000_000;
        let path = save(&page, "https://example.com/ragu", stamp, &dir).unwrap();
        assert_eq!(path, dir.join("inbox/benedetta-s-ragu-a-title-test.epub"));

        // Opened by the same engine the reader will use: the container
        // parses, the spine is there (`new` errors on an empty one), and the
        // title survives its accents and ampersand. Rendering a page needs
        // the fonts directory and is exercised by the view's own tests.
        let doc = EpubDocument::new(&path).unwrap();
        assert_eq!(doc.title().as_deref(), Some("Benedetta's Ragú — a title & test"));

        let meta = fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap()
                        .duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(mtime, stamp);

        // A second push does not resurrect the first file's clock.
        let again = save(&page, "https://example.com/ragu", stamp + 1, &dir).unwrap();
        assert_eq!(again, dir.join("inbox/benedetta-s-ragu-a-title-test-2.epub"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn slugs_are_bounded_ascii_and_never_empty() {
        assert_eq!(slug("Benedetta's Ragú"), "benedetta-s-ragu");
        assert_eq!(slug("  ...  "), "article");
        assert_eq!(slug(""), "article");
        assert_eq!(slug("œufs — brouillés"), "oeufs-brouilles");
        assert!(slug(&"long title ".repeat(30)).len() <= 65);
        // Well inside the receiver's component rules: no leading dot or dash.
        assert!(!slug(".hidden").starts_with('.'));
        assert!(!slug("-rf").starts_with('-'));
    }

    #[test]
    fn media_types_are_sniffed_from_bytes() {
        assert_eq!(media_type(b"\xFF\xD8\xFFdata"), "image/jpeg");
        assert_eq!(media_type(b"\x89PNG\r\n"), "image/png");
        assert_eq!(media_type(b"GIF89a"), "image/gif");
        assert_eq!(media_type(b"who knows"), "application/octet-stream");
    }
}
