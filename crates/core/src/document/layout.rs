//! Page-layout analysis: where the ink actually is on a page, and how to turn
//! that into one stable crop for a whole document.
//!
//! Everything here is a **pure function over slices of `Boundary`** — no
//! `Reader`, no MuPDF, no I/O, no interior state. That is deliberate: the
//! interesting part of automatic cropping is a handful of statistical
//! judgements, and keeping them out of the FFI and out of the view layer is
//! what makes them unit-testable on a host with no device and no PDF.
//!
//! The method, and the reasoning behind each step, comes from a measurement on
//! ten real papers (ezkindle `docs/plato-pdf.md` §4):
//!
//! 1. **Per page**, union the bounding boxes of the horizontal text lines and
//!    the images. That is [`content_box`].
//! 2. **Across pages**, take the 10th percentile of each minimum edge and the
//!    90th percentile of each maximum edge. That is [`aggregate_box`].
//!
//! Step 2 is the one that matters, and the union is *not* an acceptable
//! substitute for it. Per-page fit-to-width cropping was measured to swing the
//! rendered body-text size by up to +112% between adjacent pages of one paper,
//! because a page whose content happens to occupy a single column crops to
//! half the width and therefore renders at twice the scale. A percentile
//! aggregate absorbs all of that; a union absorbs none of it and is also
//! defeated by a single outlier page.
//!
//! The second half of the module is the arithmetic of a *screenful* in
//! continuous mode: where the next one starts, how tall the previous one is,
//! and which cached page to drop. Same discipline — integers and slices in,
//! integers out, so that "does a page turn ever fail to advance?" is a
//! question a unit test can answer.
//!
//! Sampling is likewise not incidental. [`sample_indices`] skips the first page
//! and the last two, because a title page and a reference list have systematically
//! different geometry from the body — and because arXiv stamps the *first* page
//! only, with a rotated line at x ≈ 10.9 pt that widens a naive content box by
//! 19%.

use crate::geom::{Boundary, LinearDir, Vec2};
use crate::metadata::Margin;

/// A text line's bounding box, plus its writing direction when the backend
/// knows it.
///
/// `dir` is fz_stext's own unit vector along the baseline: `(1, 0)` for
/// ordinary left-to-right text, `(0, ±1)` for a line rotated a quarter turn.
/// It is `None` for backends that do not expose it, in which case
/// [`TextLine::is_horizontal`] falls back to the aspect ratio.
#[derive(Debug, Clone, Copy)]
pub struct TextLine {
    pub rect: Boundary,
    pub dir: Option<Vec2>,
}

impl TextLine {
    /// A line whose direction the backend did not report.
    pub fn new(rect: Boundary) -> TextLine {
        TextLine { rect, dir: None }
    }

    pub fn with_dir(rect: Boundary, dir: Vec2) -> TextLine {
        TextLine { rect, dir: Some(dir) }
    }

    /// Whether this line runs along the page's x axis.
    ///
    /// Exact when `dir` is present and non-degenerate: a horizontal line has
    /// `|dir.x| > |dir.y|`. A degenerate direction — either component NaN, or
    /// both components ~0, which is what a single-glyph line can report —
    /// falls back to the aspect ratio, i.e. "wider than it is tall".
    ///
    /// The fallback is not merely a nicety. It is the only test available to a
    /// document backend that reports no direction at all, and on the sample it
    /// agrees with the exact test on every line that matters: the arXiv stamp
    /// is 8 pt wide and 400 pt tall.
    pub fn is_horizontal(&self) -> bool {
        match self.dir {
            Some(dir) if dir.x.is_finite() && dir.y.is_finite() &&
                         (dir.x.abs() > f32::EPSILON || dir.y.abs() > f32::EPSILON) => {
                dir.x.abs() >= dir.y.abs()
            },
            _ => self.rect.width() >= self.rect.height(),
        }
    }
}

/// Below this many usable text lines, a page tells us nothing about the
/// document's margins — it is a full-page figure, a part title, or a scan with
/// no text layer at all. Such pages are skipped rather than averaged in.
pub const MIN_LINES_PER_PAGE: usize = 4;

/// Below this many usable pages, the percentile aggregate is not a statistic,
/// it is a coin toss. The caller should fall back to ink bounding boxes.
pub const MIN_USABLE_PAGES: usize = 3;

/// Breathing room left around the aggregate box, in page points.
///
/// Text line bounding boxes are tight to the glyphs; cropping exactly to them
/// puts a descender against the panel edge and shaves the antialiasing off the
/// outermost stems.
pub const CROP_PADDING_PT: f32 = 4.0;

/// A crop must leave at least this fraction of each page dimension. Anything
/// tighter is not a margin, it is a bug, and the safe response is to render
/// the page uncropped.
pub const MIN_CROP_FRACTION: f32 = 0.25;

/// The union of a page's horizontal text lines and its images.
///
/// Returns `None` when the page has fewer than [`MIN_LINES_PER_PAGE`] usable
/// lines *and* no images — that is the "nothing to measure" case, and it is
/// deliberately not the same as an empty box.
pub fn content_box(lines: &[TextLine], images: &[Boundary]) -> Option<Boundary> {
    let usable = lines.iter()
                      .filter(|l| l.is_horizontal() && is_sane(&l.rect));

    let mut count = 0;
    let mut acc: Option<Boundary> = None;

    for line in usable {
        count += 1;
        acc = Some(union(acc, &line.rect));
    }

    if count < MIN_LINES_PER_PAGE && images.is_empty() {
        return None;
    }

    for image in images.iter().filter(|b| is_sane(b)) {
        acc = Some(union(acc, image));
    }

    acc
}

/// The document-level box: the 10th percentile of each minimum edge and the
/// 90th percentile of each maximum edge, taken independently.
///
/// Per edge rather than per box, because the edges are independent in
/// practice: the page that reaches furthest left is rarely the page that
/// reaches furthest down. Taking whole boxes at a percentile would make the
/// result depend on an arbitrary ordering.
///
/// 10/90 means one page in ten may be clipped on any given edge. That is the
/// intended trade — the alternative, the union, is dictated by whichever page
/// has the widest running head.
pub fn aggregate_box(per_page: &[Boundary]) -> Option<Boundary> {
    if per_page.is_empty() {
        return None;
    }

    let min_x = percentile(&mut per_page.iter().map(|b| b.min.x).collect::<Vec<f32>>(), 0.10);
    let min_y = percentile(&mut per_page.iter().map(|b| b.min.y).collect::<Vec<f32>>(), 0.10);
    let max_x = percentile(&mut per_page.iter().map(|b| b.max.x).collect::<Vec<f32>>(), 0.90);
    let max_y = percentile(&mut per_page.iter().map(|b| b.max.y).collect::<Vec<f32>>(), 0.90);

    if max_x <= min_x || max_y <= min_y {
        return None;
    }

    Some(Boundary::new(Vec2::new(min_x, min_y), Vec2::new(max_x, max_y)))
}

/// Turn a content box in page points into the fractional per-side margins
/// `CroppingMargins` stores.
///
/// `padding` is in page points and is applied outward on every side before the
/// conversion. Returns `None` when the result would not actually crop anything,
/// or when it would crop so much that something has gone wrong — see
/// [`MIN_CROP_FRACTION`]. `None` means "render this page uncropped", which is
/// always a correct outcome, merely a worse-looking one.
pub fn crop_margin(content: &Boundary, dims: (f32, f32), padding: f32) -> Option<Margin> {
    let (width, height) = dims;
    if !(width > 0.0 && height > 0.0) {
        return None;
    }

    let left = ((content.min.x - padding) / width).clamp(0.0, 1.0);
    let top = ((content.min.y - padding) / height).clamp(0.0, 1.0);
    let right = (1.0 - (content.max.x + padding) / width).clamp(0.0, 1.0);
    let bottom = (1.0 - (content.max.y + padding) / height).clamp(0.0, 1.0);

    if 1.0 - left - right < MIN_CROP_FRACTION || 1.0 - top - bottom < MIN_CROP_FRACTION {
        return None;
    }

    if left + right + top + bottom < f32::EPSILON {
        return None;
    }

    Some(Margin::new(top, right, bottom, left))
}

/// Which pages to sample, given how many there are and how many we want.
///
/// Skips the first page and the last two — a title page and a reference list
/// have systematically different geometry from the body, and the arXiv stamp
/// lives on page 0 — then spreads the sample evenly over what is left. A
/// document too short for that (five pages or fewer) is sampled whole, because
/// a bad crop is better than no crop and there is nothing to spare.
pub fn sample_indices(pages_count: usize, sample: usize) -> Vec<usize> {
    if pages_count == 0 || sample == 0 {
        return Vec::new();
    }

    let (first, last) = if pages_count > 5 {
        (1, pages_count - 2)
    } else {
        (0, pages_count)
    };

    let span = last - first;
    if span <= sample {
        return (first..last).collect();
    }

    (0..sample).map(|i| first + (i * span) / sample).collect()
}

/// Two text lines whose tops are within this many page points of each other
/// are one *row*.
///
/// Rows, not lines, are what an overlap is counted in. A two-column page emits
/// one fz_stext line per column at very nearly the same height, and those are
/// one line of reading, not two; counting lines would halve the overlap on
/// exactly the documents this was built for. The tolerance is deliberately
/// small — it merges columns and superscripts, not consecutive lines, which
/// are ~12 pt apart in a paper set in 10 pt type.
pub const ROW_TOLERANCE_PT: f32 = 2.0;

/// The top of the text row that is `count` rows away from `y` — upwards for
/// [`LinearDir::Backward`], downwards for [`LinearDir::Forward`].
///
/// `y` itself is expected to *be* a row top (both call sites pass a cut that
/// `find_cut` already snapped to one), so rows within `tol` of `y` are neither
/// counted nor returned; the answer is always a different row.
///
/// Saturates rather than fails: asking for the 2nd row above the 1st row of a
/// page gives the 1st row. The caller is responsible for the resulting
/// distance being sane — see [`clamp_overlap`] — because a page with two rows
/// on it would otherwise hand back an overlap of a whole screen.
pub fn row_top(rows: &[Boundary], y: f32, count: usize, dir: LinearDir, tol: f32) -> Option<f32> {
    if count == 0 {
        return None;
    }

    let mut tops: Vec<f32> = rows.iter()
                                 .filter(|r| is_sane(r))
                                 .map(|r| r.min.y)
                                 .filter(|t| match dir {
                                     LinearDir::Backward => *t < y - tol,
                                     LinearDir::Forward => *t > y + tol,
                                 })
                                 .collect();

    tops.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    tops.dedup_by(|a, b| (*a - *b).abs() <= tol);

    match dir {
        LinearDir::Backward => {
            let n = tops.len();
            tops.get(n.saturating_sub(count)).copied().or_else(|| tops.first().copied())
        },
        LinearDir::Forward => {
            tops.get(count - 1).copied().or_else(|| tops.last().copied())
        },
    }
}

/// An overlap may never eat more than half the screen, whatever the text says.
///
/// This is the only thing standing between a pathological page — two rows of
/// display type, a title page, a figure caption alone under a plate — and a
/// page turn that advances by almost nothing.
pub fn clamp_overlap(overlap: i32, available_height: i32) -> i32 {
    overlap.clamp(0, (available_height / 2).max(0))
}

/// Where the next screenful starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextScreen {
    /// Stay on this page, `.0` scaled pixels below the top of its frame.
    Same(i32),
    /// The screenful ended at the page's bottom edge: hand over to the next
    /// page, at its top.
    NextPage,
}

/// The top offset of the next screenful, given where the current one was cut.
///
/// * `cut` — the bottom of the current screenful, in scaled pixels from the
///   top of the last visible page's frame. `find_cut` has already snapped it
///   to a row top, so the row it cuts through is *not* shown.
/// * `overlap` — how far back up to start, already clamped.
/// * `current` — the offset the current screenful started at within that same
///   page, or 0 if the screenful ran on from an earlier page.
///
/// The `.max(current + 1)` is the load-bearing line: a page turn that does not
/// change the offset reads, one frame later, as "No next page" and ends the
/// book. Overlap must never be able to cause that, so progress wins over
/// overlap whenever the two conflict.
///
/// A screenful ending exactly at the page's bottom edge hands over with no
/// overlap at all. That is deliberate on both counts: there is no cut through
/// text to give the eye an anchor over — the next screenful opens on a fresh
/// page — and it leaves the last page's end-of-document path bit-identical to
/// what it was before overlap existed.
pub fn next_screen(cut: i32, pixmap_height: i32, overlap: i32, current: i32) -> NextScreen {
    if cut >= pixmap_height {
        return NextScreen::NextPage;
    }

    let target = (cut - overlap.max(0)).max(current + 1)
                                       .min((pixmap_height - 1).max(0));
    NextScreen::Same(target)
}

/// How tall the previous screenful should be so that it ends `overlap` pixels
/// into the current one.
///
/// Going backwards, the overlap is spent at the *bottom* of the screen rather
/// than the top, so it is not an offset to subtract but a smaller screen to
/// fill — which is why the backward walk in `go_to_neighbor` accumulates page
/// heights against this rather than against the real one.
pub fn previous_span(available_height: i32, overlap: i32) -> i32 {
    (available_height - overlap.max(0)).max(1)
}

/// Which cached page to drop, given the sorted cache keys and the span of
/// pages the screen is currently showing.
///
/// The policy is upstream's, unchanged: evict from whichever side of the
/// visible span holds more pages, so the two prefetched neighbours are what
/// goes first and a page that is actually on screen goes last.
///
/// Extracted because overlap widens the visible span — an overlapped
/// screenful can straddle one more page boundary than an aligned one — and
/// "can eviction ever throw away a page the screen is showing?" stops being
/// obvious at that point. It can, but only when the span alone exceeds the
/// cap, which needs pages shorter than half a screen. See the test.
pub fn eviction_candidate(keys: &[usize], first: usize, last: usize) -> Option<usize> {
    if keys.is_empty() {
        return None;
    }

    let left_count = keys.iter().filter(|k| **k < first).count();
    let right_count = keys.iter().filter(|k| **k > last).count();

    if left_count >= right_count {
        keys.first().copied()
    } else {
        keys.last().copied()
    }
}

fn is_sane(rect: &Boundary) -> bool {
    rect.min.x.is_finite() && rect.min.y.is_finite() &&
    rect.max.x.is_finite() && rect.max.y.is_finite() &&
    rect.width() > 0.0 && rect.height() > 0.0
}

fn union(acc: Option<Boundary>, rect: &Boundary) -> Boundary {
    match acc {
        None => *rect,
        Some(a) => Boundary::new(Vec2::new(a.min.x.min(rect.min.x), a.min.y.min(rect.min.y)),
                                 Vec2::new(a.max.x.max(rect.max.x), a.max.y.max(rect.max.y))),
    }
}

/// Linear-interpolated percentile of an unsorted sample. Sorts in place.
fn percentile(values: &mut Vec<f32>, q: f32) -> f32 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    let pos = q * (n - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        values[lo]
    } else {
        values[lo] + (pos - lo as f32) * (values[hi] - values[lo])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x0: f32, y0: f32, x1: f32, y1: f32) -> TextLine {
        TextLine::new(bndr![x0, y0, x1, y1])
    }

    /// A plausible body page: fourteen lines in one column, plus a running
    /// head and a folio that sit outside the text block.
    fn body_page(top: f32) -> Vec<TextLine> {
        let mut lines = vec![line(91.8, 90.0, 520.2, 100.0)];
        for i in 0..14 {
            let y = top + i as f32 * 14.0;
            lines.push(line(91.8, y, 520.2, y + 10.0));
        }
        lines
    }

    #[test]
    fn a_content_box_is_the_union_of_its_lines() {
        let lines = vec![line(100.0, 100.0, 500.0, 112.0),
                         line(90.0, 120.0, 480.0, 132.0),
                         line(100.0, 140.0, 500.0, 152.0),
                         line(110.0, 140.0, 520.0, 152.0)];
        let bx = content_box(&lines, &[]).unwrap();
        assert_eq!(bx.min.x, 90.0);
        assert_eq!(bx.min.y, 100.0);
        assert_eq!(bx.max.x, 520.0);
        assert_eq!(bx.max.y, 152.0);
    }

    #[test]
    fn images_widen_the_box_and_rescue_a_page_with_no_text() {
        let figure = bndr![60.0, 200.0, 550.0, 600.0];
        let bx = content_box(&[], &[figure]).unwrap();
        assert_eq!(bx.min.x, 60.0);
        assert_eq!(bx.max.y, 600.0);
    }

    #[test]
    fn a_page_with_too_little_text_and_no_images_measures_nothing() {
        let lines = vec![line(100.0, 100.0, 500.0, 112.0),
                         line(100.0, 120.0, 500.0, 132.0)];
        assert!(content_box(&lines, &[]).is_none());
        assert!(content_box(&[], &[]).is_none());
    }

    /// The arXiv stamp: a quarter-turn line at x = 10.9 pt running most of the
    /// page height. Including it moves the left edge from 91.8 to 10.9 pt --
    /// measured at a 19% wider crop and 16% smaller type on the first page the
    /// reader shows.
    #[test]
    fn the_arxiv_stamp_does_not_widen_the_box() {
        let mut lines = body_page(120.0);
        let stamp = TextLine::with_dir(bndr![10.9, 200.0, 19.6, 600.0], Vec2::new(0.0, -1.0));
        lines.insert(0, stamp);

        let bx = content_box(&lines, &[]).unwrap();
        assert_eq!(bx.min.x, 91.8, "the rotated stamp was not filtered out");
        assert_eq!(bx.max.x, 520.2);
    }

    /// The same stamp with no direction reported at all -- the aspect ratio has
    /// to carry it, and does: 8.7 pt wide against 400 pt tall.
    #[test]
    fn the_arxiv_stamp_is_filtered_without_a_direction_too() {
        let mut lines = body_page(120.0);
        lines.insert(0, line(10.9, 200.0, 19.6, 600.0));
        assert_eq!(content_box(&lines, &[]).unwrap().min.x, 91.8);
    }

    /// A degenerate direction -- fz_stext reports (0, 0) for some single-glyph
    /// lines -- must not be read as "vertical".
    #[test]
    fn a_degenerate_direction_falls_back_to_the_aspect_ratio() {
        let wide = TextLine::with_dir(bndr![91.8, 100.0, 520.2, 112.0], Vec2::new(0.0, 0.0));
        assert!(wide.is_horizontal());
        let tall = TextLine::with_dir(bndr![10.9, 100.0, 19.6, 500.0], Vec2::new(f32::NAN, 0.0));
        assert!(!tall.is_horizontal());
    }

    #[test]
    fn a_line_reported_horizontal_is_kept_however_it_is_shaped() {
        // One tall glyph on its own line: taller than it is wide, but the
        // direction says otherwise and the direction wins.
        let glyph = TextLine::with_dir(bndr![91.8, 100.0, 99.0, 140.0], Vec2::new(1.0, 0.0));
        assert!(glyph.is_horizontal());
    }

    #[test]
    fn degenerate_rects_are_ignored() {
        let mut lines = body_page(120.0);
        lines.push(line(0.0, 0.0, 0.0, 0.0));
        lines.push(TextLine::new(bndr![f32::NAN, 0.0, 10.0, 10.0]));
        let bx = content_box(&lines, &[]).unwrap();
        assert_eq!(bx.min.x, 91.8);
        assert_eq!(bx.min.y, 90.0);
    }

    fn body_and_one_outlier(body: usize) -> Vec<Boundary> {
        let mut boxes: Vec<Boundary> = (0..body).map(|_| bndr![91.8, 90.0, 520.2, 700.0]).collect();
        // A page whose figure bleeds into all four margins.
        boxes.push(bndr![20.0, 40.0, 590.0, 760.0]);
        boxes
    }

    /// The headline property: at the default sample of 16 pages, one outlier
    /// page does not move the crop at all.
    #[test]
    fn the_aggregate_ignores_an_outlier_page() {
        let agg = aggregate_box(&body_and_one_outlier(15)).unwrap();
        assert_eq!(agg.min.x, 91.8, "min.x was dragged out");
        assert_eq!(agg.max.x, 520.2, "max.x was dragged out");
        assert_eq!(agg.min.y, 90.0);
        assert_eq!(agg.max.y, 700.0);
    }

    /// ... and the union, for contrast, is dragged all the way out by that one
    /// page. This is the measured difference the whole design turns on, written
    /// as an assertion.
    #[test]
    fn the_union_would_have_been_dragged_out() {
        let mut acc: Option<Boundary> = None;
        for b in &body_and_one_outlier(15) {
            acc = Some(union(acc, b));
        }
        let u = acc.unwrap();
        assert_eq!(u.min.x, 20.0);
        assert_eq!(u.max.x, 590.0);
    }

    /// The honest caveat, pinned rather than hidden. A percentile interpolates,
    /// so with only ten samples the 10th percentile sits between the outlier
    /// and its neighbour and the crop moves by a few points. It is still an
    /// order of magnitude better than the union, and it is why the sample size
    /// defaults to 16 rather than to something smaller.
    #[test]
    fn a_small_sample_still_leaks_a_little_of_the_outlier() {
        let agg = aggregate_box(&body_and_one_outlier(9)).unwrap();
        assert!(agg.min.x > 80.0 && agg.min.x < 91.8, "min.x = {}", agg.min.x);
    }

    #[test]
    fn the_aggregate_of_one_page_is_that_page() {
        let agg = aggregate_box(&[bndr![91.8, 90.0, 520.2, 700.0]]).unwrap();
        assert_eq!(agg.min.x, 91.8);
        assert_eq!(agg.max.y, 700.0);
        assert!(aggregate_box(&[]).is_none());
    }

    #[test]
    fn a_zero_area_aggregate_is_refused() {
        assert!(aggregate_box(&[bndr![300.0, 400.0, 300.0, 400.0]]).is_none());
    }

    #[test]
    fn margins_are_fractions_of_the_page() {
        // US Letter, a 90 pt left margin and a 72 pt top margin, no padding.
        let m = crop_margin(&bndr![90.0, 72.0, 522.0, 720.0], (612.0, 792.0), 0.0).unwrap();
        assert!((m.left - 90.0 / 612.0).abs() < 1e-6);
        assert!((m.right - 90.0 / 612.0).abs() < 1e-6);
        assert!((m.top - 72.0 / 792.0).abs() < 1e-6);
        assert!((m.bottom - 72.0 / 792.0).abs() < 1e-6);
    }

    #[test]
    fn padding_is_applied_outward_and_clamped_at_the_page_edge() {
        let m = crop_margin(&bndr![90.0, 72.0, 522.0, 720.0], (612.0, 792.0), 10.0).unwrap();
        assert!((m.left - 80.0 / 612.0).abs() < 1e-6);
        // A box already touching the edge cannot be padded past it.
        let m = crop_margin(&bndr![0.0, 0.0, 612.0, 720.0], (612.0, 792.0), 10.0).unwrap();
        assert_eq!(m.left, 0.0);
        assert_eq!(m.right, 0.0);
    }

    #[test]
    fn a_full_page_box_is_not_a_crop() {
        assert!(crop_margin(&bndr![0.0, 0.0, 612.0, 792.0], (612.0, 792.0), 0.0).is_none());
    }

    #[test]
    fn an_absurdly_tight_box_is_refused() {
        // One stray line in the middle of an otherwise blank page.
        assert!(crop_margin(&bndr![300.0, 400.0, 320.0, 410.0], (612.0, 792.0), 0.0).is_none());
        assert!(crop_margin(&bndr![90.0, 72.0, 522.0, 720.0], (0.0, 792.0), 0.0).is_none());
    }

    #[test]
    fn sampling_skips_the_title_page_and_the_references() {
        let idx = sample_indices(20, 16);
        assert_eq!(idx.len(), 16);
        assert_eq!(idx[0], 1);
        assert!(*idx.last().unwrap() <= 17, "sampled {:?}", idx);
        assert!(idx.windows(2).all(|w| w[0] < w[1]), "not strictly increasing: {:?}", idx);
    }

    #[test]
    fn a_document_with_fewer_body_pages_than_the_sample_is_taken_whole() {
        assert_eq!(sample_indices(10, 16), (1..8).collect::<Vec<usize>>());
    }

    #[test]
    fn a_very_short_document_keeps_every_page() {
        assert_eq!(sample_indices(3, 16), vec![0, 1, 2]);
        assert_eq!(sample_indices(1, 16), vec![0]);
        assert!(sample_indices(0, 16).is_empty());
        assert!(sample_indices(50, 0).is_empty());
    }

    #[test]
    fn a_long_document_is_sampled_across_its_whole_body() {
        let idx = sample_indices(96, 16);
        assert_eq!(idx.len(), 16);
        assert_eq!(idx[0], 1);
        assert!(*idx.last().unwrap() > 80, "sampled only the front: {:?}", idx);
        assert!(*idx.last().unwrap() <= 93);
    }

    #[test]
    fn percentiles_interpolate() {
        let mut v = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&mut v, 0.0), 0.0);
        assert_eq!(percentile(&mut v, 1.0), 4.0);
        assert_eq!(percentile(&mut v, 0.5), 2.0);
        let mut v = vec![10.0, 0.0];
        assert_eq!(percentile(&mut v, 0.10), 1.0);
    }

    /// Twenty rows of 10 pt type on a 14 pt baseline, starting at y = 100.
    fn rows(n: usize) -> Vec<Boundary> {
        (0..n).map(|i| {
            let y = 100.0 + i as f32 * 14.0;
            bndr![91.8, y, 520.2, y + 10.0]
        }).collect()
    }

    #[test]
    fn a_row_above_is_a_row_above() {
        let rows = rows(20);
        // The cut sits at the top of row 10 (y = 240): two rows back is row 8.
        assert_eq!(row_top(&rows, 240.0, 1, LinearDir::Backward, ROW_TOLERANCE_PT), Some(226.0));
        assert_eq!(row_top(&rows, 240.0, 2, LinearDir::Backward, ROW_TOLERANCE_PT), Some(212.0));
        assert_eq!(row_top(&rows, 240.0, 1, LinearDir::Forward, ROW_TOLERANCE_PT), Some(254.0));
        assert_eq!(row_top(&rows, 240.0, 2, LinearDir::Forward, ROW_TOLERANCE_PT), Some(268.0));
    }

    /// Forward and backward are exact inverses, which is what makes a
    /// Next-then-Previous pair land back where it started.
    #[test]
    fn stepping_back_and_forward_is_symmetric() {
        let rows = rows(20);
        let up = row_top(&rows, 240.0, 2, LinearDir::Backward, ROW_TOLERANCE_PT).unwrap();
        assert_eq!(row_top(&rows, up, 2, LinearDir::Forward, ROW_TOLERANCE_PT), Some(240.0));
    }

    /// The reason this counts rows and not lines: a two-column page emits two
    /// lines per row, and an overlap of two lines would be one row of reading.
    #[test]
    fn the_two_columns_of_a_row_count_once() {
        let mut two_col = Vec::new();
        for i in 0..20 {
            let y = 100.0 + i as f32 * 14.0;
            two_col.push(bndr![54.0, y, 290.0, y + 10.0]);
            // The right column's baseline is a hair off, as it is in practice.
            two_col.push(bndr![307.0, y + 0.4, 543.0, y + 10.4]);
        }
        assert_eq!(row_top(&two_col, 240.0, 2, LinearDir::Backward, ROW_TOLERANCE_PT), Some(212.0));
    }

    #[test]
    fn asking_for_more_rows_than_there_are_saturates() {
        let three = rows(3);
        assert_eq!(row_top(&three, 128.0, 9, LinearDir::Backward, ROW_TOLERANCE_PT), Some(100.0));
        assert_eq!(row_top(&three, 100.0, 9, LinearDir::Forward, ROW_TOLERANCE_PT), Some(128.0));
        assert!(row_top(&[], 100.0, 2, LinearDir::Backward, ROW_TOLERANCE_PT).is_none());
        assert!(row_top(&rows(20), 240.0, 0, LinearDir::Backward, ROW_TOLERANCE_PT).is_none());
        // Nothing above the first row, nothing below the last.
        assert!(row_top(&three, 100.0, 1, LinearDir::Backward, ROW_TOLERANCE_PT).is_none());
        assert!(row_top(&three, 128.0, 1, LinearDir::Forward, ROW_TOLERANCE_PT).is_none());
    }

    #[test]
    fn an_overlap_never_eats_more_than_half_the_screen() {
        assert_eq!(clamp_overlap(40, 1448), 40);
        assert_eq!(clamp_overlap(900, 1448), 724);
        assert_eq!(clamp_overlap(-5, 1448), 0);
        assert_eq!(clamp_overlap(40, 0), 0);
    }

    #[test]
    fn a_screenful_advances_by_its_height_less_the_overlap() {
        // A page 4000 px tall, a screen of 1448, an overlap of 40.
        assert_eq!(next_screen(1448, 4000, 40, 0), NextScreen::Same(1408));
        assert_eq!(next_screen(2856, 4000, 40, 1408), NextScreen::Same(2816));
        // With the overlap off, the old arithmetic exactly.
        assert_eq!(next_screen(1448, 4000, 0, 0), NextScreen::Same(1448));
    }

    #[test]
    fn a_screenful_that_ends_at_the_page_edge_hands_over() {
        assert_eq!(next_screen(4000, 4000, 40, 2816), NextScreen::NextPage);
        // Defensive: a cut past the edge is still a hand-over, not a negative
        // offset into the next page.
        assert_eq!(next_screen(4200, 4000, 40, 2816), NextScreen::NextPage);
    }

    /// The property that matters more than the overlap itself: every turn that
    /// stays on the page moves forward. A turn that does not is read as the
    /// end of the document.
    #[test]
    fn a_turn_always_advances() {
        for cut in 1..600 {
            for overlap in [0, 1, 40, 599, 100_000] {
                for current in 0..cut {
                    match next_screen(cut, 4000, overlap, current) {
                        NextScreen::Same(off) => {
                            assert!(off > current, "cut {} overlap {} current {} -> {}",
                                    cut, overlap, current, off);
                            assert!(off < 4000);
                        },
                        NextScreen::NextPage => panic!("handed over mid-page"),
                    }
                }
            }
        }
    }

    /// A page shorter than the overlap: the screenful is mostly the *next*
    /// page, and the turn still has to advance.
    #[test]
    fn a_page_shorter_than_the_overlap_still_turns() {
        assert_eq!(next_screen(10, 12, 40, 0), NextScreen::Same(1));
        assert_eq!(next_screen(1, 2, 40, 0), NextScreen::Same(1));
    }

    #[test]
    fn the_previous_screenful_is_shortened_by_the_overlap() {
        assert_eq!(previous_span(1448, 40), 1408);
        assert_eq!(previous_span(1448, 0), 1448);
        // Never zero: a zero-height screen never terminates the backward walk.
        assert_eq!(previous_span(40, 40), 1);
        assert_eq!(previous_span(40, 4000), 1);
    }

    /// The cache holds three pages. An aligned screenful spans one or two; an
    /// overlapped one can span one more. Eviction must take the prefetched
    /// neighbours first in every case.
    #[test]
    fn eviction_takes_the_neighbours_before_the_screen() {
        // Prefetch has just added 4 and 8 around a screen spanning 5..=7.
        assert_eq!(eviction_candidate(&[4, 5, 6, 7, 8], 5, 7), Some(4));
        assert_eq!(eviction_candidate(&[5, 6, 7, 8], 5, 7), Some(8));
        // A screen on one page, both neighbours cached: symmetric, left first.
        assert_eq!(eviction_candidate(&[4, 5, 6], 5, 5), Some(4));
        assert_eq!(eviction_candidate(&[], 0, 0), None);
    }

    /// The honest limit, pinned rather than assumed away: once the screen
    /// spans more pages than the cache holds, eviction has no choice but to
    /// drop a visible one, and the next `update` rasterises it again. It takes
    /// four pages on one screen, i.e. pages under half a screen tall, which
    /// overlap alone cannot produce.
    #[test]
    fn a_screen_wider_than_the_cache_does_thrash() {
        assert_eq!(eviction_candidate(&[5, 6, 7, 8], 5, 8), Some(5));
    }

    /// Fixture-backed tests over three real papers.
    ///
    /// `test-data/line-boxes.json` holds the fz_stext line and image boxes
    /// extracted from `gepa.pdf` (single column), `demo search predict.pdf`
    /// and `wikipedia assist.pdf` (both two column, both arXiv-stamped) by
    /// `test-data/gen-line-boxes.py`. The PDFs themselves are deliberately not
    /// committed: they are megabytes each, they are not ours to redistribute,
    /// and the boxes are the entire input to everything in this module.
    mod fixtures {
        use super::*;
        use serde::Deserialize;

        const FIXTURE: &str = include_str!("../../test-data/line-boxes.json");

        #[derive(Deserialize)]
        struct Fixture {
            sample: usize,
            documents: Vec<Doc>,
        }

        #[derive(Deserialize)]
        struct Doc {
            name: String,
            pages_count: usize,
            dims: (f32, f32),
            pages: Vec<Page>,
        }

        #[derive(Deserialize)]
        struct Page {
            index: usize,
            /// `[x0, y0, x1, y1, dir_x, dir_y]`.
            lines: Vec<[f32; 6]>,
            images: Vec<[f32; 4]>,
        }

        impl Page {
            fn text_lines(&self) -> Vec<TextLine> {
                self.lines.iter()
                    .map(|l| TextLine::with_dir(bndr![l[0], l[1], l[2], l[3]],
                                                Vec2::new(l[4], l[5])))
                    .collect()
            }

            fn image_boxes(&self) -> Vec<Boundary> {
                self.images.iter().map(|b| bndr![b[0], b[1], b[2], b[3]]).collect()
            }
        }

        fn load() -> Fixture {
            serde_json::from_str(FIXTURE).unwrap()
        }

        /// The whole Phase-A pipeline, on real papers: sample the body pages,
        /// take each one's content box, aggregate. The assertions are the
        /// measured ranges from `docs/plato-pdf.md` §4.1 -- 65-82% of page
        /// width, 57-70% of page area -- widened only where a single document
        /// sits just outside (gepa is 56.6% of area).
        #[test]
        fn the_aggregate_of_a_real_paper_lands_where_it_was_measured() {
            let fixture = load();
            assert_eq!(fixture.documents.len(), 3);

            for doc in &fixture.documents {
                let wanted = sample_indices(doc.pages_count, fixture.sample);
                let boxes: Vec<Boundary> = doc.pages.iter()
                    .filter(|p| wanted.contains(&p.index))
                    .filter_map(|p| content_box(&p.text_lines(), &p.image_boxes()))
                    .collect();

                assert!(boxes.len() >= MIN_USABLE_PAGES,
                        "{}: only {} usable pages", doc.name, boxes.len());

                let agg = aggregate_box(&boxes)
                    .unwrap_or_else(|| panic!("{}: no aggregate", doc.name));
                let (w, h) = doc.dims;
                let width = agg.width() / w;
                let area = width * agg.height() / h;

                assert!((0.60..=0.85).contains(&width),
                        "{}: aggregate is {:.1}% of page width", doc.name, 100.0 * width);
                assert!((0.55..=0.70).contains(&area),
                        "{}: aggregate is {:.1}% of page area", doc.name, 100.0 * area);

                let margin = crop_margin(&agg, doc.dims, CROP_PADDING_PT)
                    .unwrap_or_else(|| panic!("{}: the crop was refused", doc.name));
                assert!(margin.left > 0.0 && margin.right > 0.0,
                        "{}: {:?}", doc.name, margin);
            }
        }

        /// §4.2, on the real thing rather than on a fixture drawn by hand.
        /// All three papers carry the vertical `arXiv:NNNN.NNNNN` stamp at
        /// x = 10.9 pt on page 0. Without the rotation filter every one of
        /// them crops from 10.9 instead of from its real left margin.
        #[test]
        fn the_arxiv_stamp_is_filtered_on_every_sampled_first_page() {
            for doc in &load().documents {
                let page = doc.pages.iter().find(|p| p.index == 0).unwrap();
                let lines = page.text_lines();
                let images = page.image_boxes();

                let filtered = content_box(&lines, &images).unwrap();
                let unfiltered = {
                    let all: Vec<TextLine> = lines.iter()
                        .map(|l| TextLine::with_dir(l.rect, Vec2::new(1.0, 0.0)))
                        .collect();
                    content_box(&all, &images).unwrap()
                };

                assert!((unfiltered.min.x - 10.9).abs() < 0.5,
                        "{}: expected the stamp at 10.9, got {}", doc.name, unfiltered.min.x);
                assert!(filtered.min.x > 50.0,
                        "{}: the stamp survived the filter at {}", doc.name, filtered.min.x);
            }
        }

        /// Every sampled body page of a real paper has enough text to measure.
        /// If this ever fails on new fixtures it is `MIN_LINES_PER_PAGE` that
        /// is wrong, not the paper.
        #[test]
        fn real_body_pages_are_usable() {
            for doc in &load().documents {
                for page in doc.pages.iter().filter(|p| p.index > 0) {
                    assert!(content_box(&page.text_lines(), &page.image_boxes()).is_some(),
                            "{} page {}: not usable", doc.name, page.index);
                }
            }
        }
    }
}
