#!/usr/bin/env python3
"""Extract fz_stext line/image boxes from PDFs into `line-boxes.json`.

The fixture, not the PDFs, is what gets committed: the papers are Adriaan's,
they are megabytes each, and the *only* thing the layout analysis consumes is
the boxes. This script walks exactly the structures `PdfPage::lines()` and
`PdfPage::images()` walk in `document/pdf.rs`, so the fixture is the same input
the real code would see.

    pip install pymupdf
    python3 crates/core/test-data/gen-line-boxes.py OUT.json PAPER.pdf ...

Page 0 is always included even though `layout::sample_indices` skips it,
because page 0 is where the arXiv stamp lives and the rotation filter has to be
exercised on the real thing rather than on a fixture someone drew by hand.
Coordinates are rounded to 0.1 pt, which is a tenth of the smallest difference
any of this cares about and roughly halves the file.
"""

import json
import sys

import pymupdf

SAMPLE = 16


def sample_indices(pages_count, sample=SAMPLE):
    """Mirror of `layout::sample_indices`; kept in step by the Rust tests."""
    if pages_count == 0 or sample == 0:
        return []
    if pages_count > 5:
        first, last = 1, pages_count - 2
    else:
        first, last = 0, pages_count
    span = last - first
    if span <= sample:
        return list(range(first, last))
    return [first + (i * span) // sample for i in range(sample)]


def page_boxes(page):
    lines, images = [], []
    d = page.get_text("rawdict")
    for block in d["blocks"]:
        if block["type"] == 1:                      # FZ_PAGE_BLOCK_IMAGE
            images.append([round(v, 1) for v in block["bbox"]])
            continue
        for line in block["lines"]:                 # FZ_PAGE_BLOCK_TEXT
            dx, dy = line["dir"]
            lines.append([round(v, 1) for v in line["bbox"]] +
                         [round(dx, 4), round(dy, 4)])
    return lines, images


def main(out_path, pdf_paths):
    docs = []
    for path in pdf_paths:
        doc = pymupdf.open(path)
        n = len(doc)
        indices = sorted(set([0] + sample_indices(n)))
        pages = []
        for i in indices:
            page = doc[i]
            lines, images = page_boxes(page)
            pages.append({"index": i, "lines": lines, "images": images})
        rect = doc[1].rect if n > 1 else doc[0].rect
        docs.append({
            "name": doc.name.rsplit("/", 1)[-1].rsplit(".", 1)[0],
            "pages_count": n,
            "dims": [round(rect.width, 1), round(rect.height, 1)],
            "pages": pages,
        })
        doc.close()

    with open(out_path, "w") as f:
        json.dump({"sample": SAMPLE, "documents": docs}, f, separators=(",", ":"))
        f.write("\n")
    print(f"{out_path}: {len(docs)} documents, "
          f"{sum(len(p['lines']) for d in docs for p in d['pages'])} lines")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2:])
