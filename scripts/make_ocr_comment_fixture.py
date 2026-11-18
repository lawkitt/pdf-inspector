#!/usr/bin/env python3
"""Generate the OCR comment fixture ``tests/fixtures/scanned_comments.pdf``.

A synthetic one-page "scan": the page content is a single grayscale image
with two lines of body text drawn in a blocky bitmap font, and no text layer,
so the page is read by OCR. On top of it sit annotations whose appearances
carry distinctive words:

    FreeText   "Reviewer remark zebra"     drawn by its appearance stream
    Text       "Sticky note walrus"        a note whose appearance shows its
                                           text beside the icon
    Popup      of the note                 open, with the text in its appearance
                                           (PDFium never draws a file's popups;
                                           other renderers do)
    Highlight  over "SCANNED PAGE", comment "Highlight comment heron"
    Stamp      "APPROVED"                  drawn by its appearance stream

The comment-type annotations (notes, FreeText, popups, text markup) are left
out of the OCR render, so "zebra", "walrus" and "heron" must not reach the
recognized Markdown, while the stamp's "APPROVED" and the body text must.

Only the standard library is needed (the PDF writer is shared with
``make_font_metadata_fixtures.py``):

    python3 scripts/make_ocr_comment_fixture.py

The output is deterministic: running the script again reproduces the
committed file byte for byte.
"""

from __future__ import annotations

import argparse
import sys
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from make_font_metadata_fixtures import PdfWriter, pdf_escape  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests" / "fixtures" / "scanned_comments.pdf"

PAGE_WIDTH, PAGE_HEIGHT = 612, 792
# The "scan" resolution: 100 pixels per inch over a Letter page.
DPI = 100
IMAGE_WIDTH = PAGE_WIDTH * DPI // 72
IMAGE_HEIGHT = PAGE_HEIGHT * DPI // 72

# A 5x7 bitmap font, one string of five columns per row, for the letters the
# body text uses.
GLYPHS = {
    "A": ("01110", "10001", "10001", "11111", "10001", "10001", "10001"),
    "B": ("11110", "10001", "10001", "11110", "10001", "10001", "11110"),
    "C": ("01110", "10001", "10000", "10000", "10000", "10001", "01110"),
    "D": ("11110", "10001", "10001", "10001", "10001", "10001", "11110"),
    "E": ("11111", "10000", "10000", "11110", "10000", "10000", "11111"),
    "G": ("01110", "10001", "10000", "10111", "10001", "10001", "01111"),
    "H": ("10001", "10001", "10001", "11111", "10001", "10001", "10001"),
    "I": ("01110", "00100", "00100", "00100", "00100", "00100", "01110"),
    "L": ("10000", "10000", "10000", "10000", "10000", "10000", "11111"),
    "N": ("10001", "11001", "10101", "10011", "10001", "10001", "10001"),
    "O": ("01110", "10001", "10001", "10001", "10001", "10001", "01110"),
    "P": ("11110", "10001", "10001", "11110", "10000", "10000", "10000"),
    "R": ("11110", "10001", "10001", "11110", "10100", "10010", "10001"),
    "S": ("01111", "10000", "10000", "01110", "00001", "00001", "11110"),
    "T": ("11111", "00100", "00100", "00100", "00100", "00100", "00100"),
    "U": ("10001", "10001", "10001", "10001", "10001", "10001", "01110"),
    "V": ("10001", "10001", "10001", "10001", "10001", "01010", "00100"),
    "X": ("10001", "10001", "01010", "00100", "01010", "10001", "10001"),
    "Y": ("10001", "10001", "01010", "00100", "00100", "00100", "00100"),
    " ": ("00000",) * 7,
}

# Pixels per font dot, and the left margin and top of each body line, in
# image pixels.
DOT = 4
LEFT_PX = 100
BODY_LINES = ((130, "SCANNED PAGE BODY TEXT"), (200, "THE BUYER PAYS ON DELIVERY"))


def body_image() -> bytes:
    """The page image: 8-bit gray rows, white with black body text."""
    rows = [bytearray(b"\xff" * IMAGE_WIDTH) for _ in range(IMAGE_HEIGHT)]
    for top, text in BODY_LINES:
        for index, char in enumerate(text):
            left = LEFT_PX + index * 6 * DOT
            for row, bits in enumerate(GLYPHS[char]):
                for column, bit in enumerate(bits):
                    if bit != "1":
                        continue
                    for y in range(top + row * DOT, top + (row + 1) * DOT):
                        x = left + column * DOT
                        rows[y][x : x + DOT] = b"\x00" * DOT
    return b"".join(bytes(row) for row in rows)


def pixel_box(top: int, text: str, start: int, length: int) -> tuple[float, float, float, float]:
    """PDF ``(x0, y0, x1, y1)`` around ``length`` characters of a body line
    from character ``start``, with a pixel of margin."""
    scale = 72 / DPI
    x0 = (LEFT_PX + start * 6 * DOT - 2) * scale
    x1 = (LEFT_PX + (start + length) * 6 * DOT - DOT + 2) * scale
    y_top = PAGE_HEIGHT - (top - 2) * scale
    y_bottom = PAGE_HEIGHT - (top + 7 * DOT + 2) * scale
    return (x0, y_bottom, x1, y_top)


def num(value: float) -> str:
    """A PDF number with at most three decimals and no trailing zeros."""
    text = f"{value:.3f}".rstrip("0").rstrip(".")
    return text if text not in ("", "-0") else "0"


def rect(box) -> str:
    return "[" + " ".join(num(v) for v in box) + "]"


def quad_points(box) -> str:
    """One quad in Acrobat's order: upper left, upper right, lower left, lower right."""
    x0, y0, x1, y1 = box
    return "[" + " ".join(num(v) for v in (x0, y1, x1, y1, x0, y0, x1, y0)) + "]"


def build_pdf() -> bytes:
    writer = PdfWriter()
    catalog = writer.reserve()
    pages = writer.reserve()
    info = writer.add(
        b"<< /Title (Scanned page with comments) /Subject (Synthetic fixture: comment"
        b" annotations over a scanned page) /Producer (scripts/make_ocr_comment_fixture.py) >>"
    )
    helvetica = writer.add(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
    )
    helvetica_bold = writer.add(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
    )
    page = writer.reserve()

    image_data = zlib.compress(body_image(), 9)
    image = writer.add(
        (
            f"<< /Type /XObject /Subtype /Image /Width {IMAGE_WIDTH} /Height {IMAGE_HEIGHT}"
            f" /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode"
            f" /Length {len(image_data)} >>\nstream\n"
        ).encode("latin-1")
        + image_data
        + b"\nendstream"
    )
    contents = writer.stream("", f"q {PAGE_WIDTH} 0 0 {PAGE_HEIGHT} 0 0 cm /Im1 Do Q".encode("latin-1"))

    def form(width: float, height: float, font: int, ops: str) -> int:
        return writer.stream(
            f"/Type /XObject /Subtype /Form /BBox [0 0 {num(width)} {num(height)}]"
            f" /Resources << /Font << /F1 {font} 0 R >> >>",
            ops.encode("latin-1"),
        )

    def add(entries: str) -> int:
        return writer.add(f"<< /Type /Annot {entries} >>".encode("latin-1"))

    free_text = (72, 520, 372, 556)
    free_text_ap = form(
        300,
        36,
        helvetica,
        "1 g 0 0 300 36 re f 0 G 1 w 0.5 0.5 299 35 re S"
        f" BT 0 g /F1 18 Tf 10 12 Td ({pdf_escape('Reviewer remark zebra')}) Tj ET",
    )
    free_text_annot = add(
        f"/Subtype /FreeText /Rect {rect(free_text)} /P {page} 0 R /T (Reviewer)"
        " /M (D:20261118090000Z) /Contents (Reviewer remark zebra) /DA (/Helv 18 Tf 0 g)"
        f" /AP << /N {free_text_ap} 0 R >>"
    )

    note = writer.reserve()
    popup_box = (300, 380, 560, 440)
    popup_ap = form(
        260,
        60,
        helvetica,
        "1 1 0.8 rg 0 0 260 60 re f 0 G 1 w 0.5 0.5 259 59 re S"
        f" BT 0 g /F1 18 Tf 10 22 Td ({pdf_escape('Sticky note walrus')}) Tj ET",
    )
    popup = add(
        f"/Subtype /Popup /Rect {rect(popup_box)} /P {page} 0 R /Parent {note} 0 R /Open true"
        f" /AP << /N {popup_ap} 0 R >>"
    )
    note_box = (392, 470, 592, 500)
    note_ap = form(
        200,
        30,
        helvetica,
        "1 1 0 rg 0 0 30 30 re f 0 G 1 w 0.5 0.5 29 29 re S"
        f" BT 0 g /F1 16 Tf 38 9 Td ({pdf_escape('Sticky note walrus')}) Tj ET",
    )
    writer.set(
        note,
        (
            f"<< /Type /Annot /Subtype /Text /Rect {rect(note_box)} /P {page} 0 R"
            " /Name /Comment /C [1 1 0] /T (Reviewer) /M (D:20261118091000Z)"
            f" /Contents (Sticky note walrus) /Open true /Popup {popup} 0 R"
            f" /AP << /N {note_ap} 0 R >> >>"
        ).encode("latin-1"),
    )

    top, text = BODY_LINES[0]
    marked = pixel_box(top, text, 0, len("SCANNED PAGE"))
    highlight = add(
        f"/Subtype /Highlight /Rect {rect(marked)} /P {page} 0 R"
        f" /QuadPoints {quad_points(marked)} /C [1 1 0] /T (Reviewer)"
        " /M (D:20261118092000Z) /Contents (Highlight comment heron)"
    )

    stamp_box = (72, 280, 292, 340)
    stamp_ap = form(
        220,
        60,
        helvetica_bold,
        "0.8 0 0 RG 3 w 1.5 1.5 217 57 re S"
        " BT 0.8 0 0 rg /F1 32 Tf 18 18 Td (APPROVED) Tj ET",
    )
    stamp = add(
        f"/Subtype /Stamp /Rect {rect(stamp_box)} /P {page} 0 R /Name /Approved"
        f" /T (Reviewer) /M (D:20261118093000Z) /AP << /N {stamp_ap} 0 R >>"
    )

    annots = " ".join(f"{ref} 0 R" for ref in (free_text_annot, note, popup, highlight, stamp))
    writer.set(
        page,
        (
            f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 {PAGE_WIDTH} {PAGE_HEIGHT}]"
            f" /Resources << /XObject << /Im1 {image} 0 R >> >> /Contents {contents} 0 R"
            f" /Annots [{annots}] >>"
        ).encode("latin-1"),
    )
    writer.set(pages, f"<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>".encode("latin-1"))
    writer.set(catalog, f"<< /Type /Catalog /Pages {pages} 0 R >>".encode("latin-1"))
    return writer.build(catalog, info)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--output", type=Path, default=FIXTURE)
    args = parser.parse_args(argv)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(build_pdf())
    print(f"wrote {args.output} ({args.output.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
