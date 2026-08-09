//! Headless document → PNG renderer: the ezkindle fork's smoke test.
//!
//! ```text
//! plato-harness <book.epub|paper.pdf> <out.png> [page] [font-size-pt]
//! ```
//!
//! Everything is fixed at the Paperwhite 3's real panel geometry (1072x1448
//! at 300 dpi, 8-bit grayscale) so that the same command on the host and on
//! armv7-under-qemu must produce the same file. No settings are read, no
//! environment is consulted, nothing is written but the PNG: the only
//! difference between the two runs is the instruction set.
//!
//! Paths that must resolve relative to the current directory, because Plato
//! resolves them that way: `css/epub.css`, `hyphenation-patterns/`, `fonts/`.
//! Run it from the repository root.

use std::path::Path;
use std::process;

use anyhow::{Context, Error, format_err};
use plato_core::document::{Location, open};
use plato_core::framebuffer::Framebuffer;

/// The PW3 panel, from ezkindle `docs/plato-port.md`.
const WIDTH: u32 = 1072;
const HEIGHT: u32 = 1448;
const DPI: u16 = 300;
/// Grayscale: one sample per pixel, matching an e-ink framebuffer.
const SAMPLES: usize = 1;

fn main() {
    if let Err(err) = run() {
        eprintln!("plato-harness: {:#}", err);
        process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        return Err(format_err!(
            "usage: {} <book.epub|paper.pdf> <out.png> [page] [font-size-pt]",
            args.first().map(String::as_str).unwrap_or("plato-harness")));
    }
    let input = &args[1];
    let output = &args[2];
    let page: usize = args.get(3).map_or(Ok(0), |s| s.parse())
        .context("page must be a number")?;
    let font_size: f32 = args.get(4).map_or(Ok(11.0), |s| s.parse())
        .context("font size must be a number")?;

    if !Path::new("css/epub.css").exists() {
        eprintln!("plato-harness: warning: no css/epub.css in the current \
                   directory; run from the repository root or the render \
                   will not match.");
    }

    // `document::open` sniffs the file kind and dispatches: EPUB and HTML to
    // their own backends, everything else -- PDF included -- to MuPDF via
    // `PdfOpener`. Every call below is on the `Document` trait, so nothing
    // else in the harness has to know which one it got.
    let mut doc = open(input)
        .ok_or_else(|| format_err!("can't open {}", input))?;
    doc.layout(WIDTH, HEIGHT, font_size, DPI);

    // `Location::Exact` takes a byte *offset* for a reflowable document, not
    // a page index -- Exact(0), Exact(3) and Exact(40) all land on the first
    // page, which makes for a very convincing smoke test that renders the
    // same image every time. Step with `Next` instead. (For a paginated
    // document -- a PDF -- `Exact` *is* the page index, and `Next` steps it
    // just the same, so one loop serves both.)
    let mut offset = doc.resolve_location(Location::Exact(0))
        .ok_or_else(|| format_err!("the document has no first page"))?;
    for n in 0..page {
        offset = doc.resolve_location(Location::Next(offset))
            .ok_or_else(|| format_err!("the document ends after page {}", n))?;
    }

    // A reflowable document has already been laid out at the panel's geometry,
    // so it renders at scale 1.0. A paginated one -- a PDF -- ignores `layout`
    // and rasterises its own page box in points, which for US Letter is
    // 612x792: a third of the panel's pixels. Fit it to the panel width, which
    // is what `Reader` does for `ZoomMode::FitToWidth`, so the harness output
    // is at device geometry for both kinds.
    let scale = if doc.is_reflowable() {
        1.0
    } else {
        let (w, _) = doc.dims(offset)
            .ok_or_else(|| format_err!("no dimensions for page {}", offset))?;
        WIDTH as f32 / w
    };

    let (pixmap, _) = doc.pixmap(Location::Exact(offset), scale, SAMPLES)
        .ok_or_else(|| format_err!("no page at offset {}", offset))?;
    pixmap.save(output)?;

    println!("plato-harness: {} page {} -> {} ({}x{}, {} bytes)",
             input, page, output, pixmap.width, pixmap.height,
             pixmap.data.len());
    Ok(())
}
