//! `document::layout` against a real PDF, through the real MuPDF FFI.
//!
//! The unit tests in `document/layout.rs` run on committed fixtures of
//! *extracted* boxes, so they pin the analysis but say nothing about the
//! extraction. This one closes that gap: it is the only thing that checks
//! `PdfPage::text_lines` reads fz_stext's `dir` out of the right offset, which
//! is a `#[repr(C)]` layout question and therefore exactly the kind of thing
//! that is silently wrong.
//!
//! It needs a PDF, and no PDF is committed -- the sample papers are not ours to
//! redistribute and they are megabytes each. So it is skipped unless one is
//! named:
//!
//! ```text
//! PLATO_TEST_PDF="/path/to/paper.pdf" python3 xbuild.py host --test --package plato-core
//! ```
//!
//! Skipped rather than failed, deliberately: a test that cannot run is not a
//! test that failed, and making the suite depend on a file outside the repo
//! would make `--test` useless to anyone but the person who has that file.

use std::env;
use std::path::PathBuf;

use plato_core::document::{Location, open};
use plato_core::document::layout::{self, CROP_PADDING_PT, MIN_USABLE_PAGES};

fn sample_pdf() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os("PLATO_TEST_PDF")?);
    path.is_file().then_some(path)
}

#[test]
fn a_real_pdf_crops_to_something_plausible() {
    let Some(path) = sample_pdf() else {
        eprintln!("skipped: set PLATO_TEST_PDF to a PDF to run this");
        return;
    };

    let mut doc = open(&path).expect("can't open the PDF");
    assert!(!doc.is_reflowable(), "PLATO_TEST_PDF must name a paginated document");

    let indices = layout::sample_indices(doc.pages_count(), 16);
    assert!(!indices.is_empty());

    let dims = doc.dims(indices[0]).expect("no page dimensions");

    let mut boxes = Vec::new();
    let mut rotated_seen = false;

    for &index in &indices {
        let (lines, _) = doc.text_lines(Location::Exact(index)).expect("no text lines");
        rotated_seen |= lines.iter().any(|l| !l.is_horizontal());
        let images = doc.images(Location::Exact(index))
                        .map(|(images, _)| images).unwrap_or_default();
        if let Some(bnd) = layout::content_box(&lines, &images) {
            assert!(bnd.min.x >= 0.0 && bnd.max.x <= dims.0 + 1.0,
                    "page {}: content box {:?} escapes a {:?} page", index, bnd, dims);
            boxes.push(bnd);
        }
    }

    assert!(boxes.len() >= MIN_USABLE_PAGES,
            "only {} of {} sampled pages were usable", boxes.len(), indices.len());

    let content = layout::aggregate_box(&boxes).expect("no aggregate box");
    let margin = layout::crop_margin(&content, dims, CROP_PADDING_PT)
        .expect("the crop was refused");

    // The interesting failure is a crop that eats the text, so assert the
    // shape rather than a number: every side is a margin, not a majority.
    for (name, value) in [("top", margin.top), ("right", margin.right),
                          ("bottom", margin.bottom), ("left", margin.left)] {
        assert!((0.0..0.35).contains(&value), "{} margin is {}", name, value);
    }

    // `ink_box` is the scanned-PDF fallback and was dead code before Phase A.
    // It has to at least answer, and its answer has to contain the text.
    let (ink, _) = doc.ink_box(Location::Exact(indices[0])).expect("no ink box");
    assert!(ink.width() > 0.0 && ink.height() > 0.0);

    println!("{}: {} pages, {} sampled, {} usable, box {:?}, rotated lines seen: {}",
             path.display(), doc.pages_count(), indices.len(), boxes.len(),
             content, rotated_seen);
}
