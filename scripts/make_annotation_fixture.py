#!/usr/bin/env python3
"""Generate the annotation fixture ``tests/fixtures/text_annotations.pdf``.

A synthetic two-page document whose annotations exercise the text-bearing
annotations pdf-inspector reports beside the Markdown (``annotations`` on the
per-page and OCR results): a sticky note with its popup, a FreeText box, a
highlight with a comment and a reply in Russian, a review state that resolves
the note, markup with rich text only, a highlight without a comment, and
entries that must be skipped (a malformed rectangle, an entry that is not a
dictionary).

Page 1 has an offset CropBox ([36 36 576 756] inside a Letter MediaBox), so
reported geometry is shifted by (36, 36); page 2 has no CropBox. Page text is
Helvetica 12 pt from x = 72 with baselines 700, 680 and 660. Quads are placed
from Helvetica's widths and span 3 pt below to 10 pt above the baseline.

Page 1 /Annots, in order:

    A1  Text       Alice, /M and /CreationDate, "Check the delivery term.", popup P1
    P1  Popup      of A1 (not reported)
    A2  FreeText   Bob, "Draft for review", with an appearance stream
    A3  Highlight  "thirty days", Alice, /CreationDate only, a comment, popup P3
    P3  Popup      of A3 (not reported)
    A4  Text       reply to A3: author "Борис", "Да, рабочих дней."
    A5  Text       Review state Completed in reply to A1 (resolves A1; not reported)
    A6  Highlight  "highlight", no /Contents (not reported)
    A7  Underline  "Payment is due", author "Ирина", /RC only (with Cyrillic)
    A8  Highlight  /Rect is a string (malformed: skipped)
    A9  StrikeOut  "shall", "Delete"
    A10 Squiggly   "invoice", "Wrong word?"
    --  42         an indirect integer, not a dictionary (skipped)

Page 2 /Annots:

    B1  Text       Bob, "Second page note"
    B2  Text       Review state Accepted in reply to B1
    B3  Text       Review state Rejected in reply to B1, dated later (B1 stays
                   unresolved: the latest review state wins)

Only the standard library is needed (the PDF writer is shared with
``make_font_metadata_fixtures.py``):

    python3 scripts/make_annotation_fixture.py

The output is deterministic: running the script again reproduces the
committed file byte for byte.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from make_font_metadata_fixtures import PdfWriter, pdf_escape  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests" / "fixtures" / "text_annotations.pdf"

FONT_SIZE = 12
LEFT = 72

# Helvetica's advances (Adobe Core 14 AFM) for the characters the pages show.
HELVETICA_WIDTHS = {
    " ": 278, ",": 278, ".": 278, "D": 722, "P": 667, "S": 667, "T": 611,
    "a": 556, "b": 556, "c": 500, "d": 556, "e": 556, "f": 278, "g": 556,
    "h": 556, "i": 222, "l": 222, "m": 833, "n": 556, "o": 556, "p": 556,
    "r": 333, "s": 500, "t": 278, "u": 556, "v": 500, "w": 722, "y": 500,
}

PAGE1_LINES = (
    (700, "The supplier shall deliver the goods within thirty days."),
    (680, "Payment is due upon receipt of the invoice."),
    (660, "This clause has a highlight with no comment."),
)
PAGE2_LINES = ((700, "Second page with a note."),)


def text_width(text: str) -> float:
    """The width of ``text`` set in Helvetica at the fixture's size."""
    return sum(HELVETICA_WIDTHS[ch] for ch in text) * FONT_SIZE / 1000


def phrase_box(line: str, baseline: int, phrase: str) -> tuple[float, float, float, float]:
    """``(x0, y0, x1, y1)`` around ``phrase`` where ``line`` sets it."""
    start = line.index(phrase)
    x0 = LEFT + text_width(line[:start])
    return (x0, baseline - 3, x0 + text_width(phrase), baseline + 10)


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


def text_string(text: str) -> str:
    """A PDF text string: a literal for ASCII, UTF-16BE with a byte order mark otherwise."""
    if text.isascii():
        return f"({pdf_escape(text)})"
    return "<FEFF" + text.encode("utf-16-be").hex().upper() + ">"


def content_stream(lines) -> bytes:
    ops = [
        f"BT /F1 {FONT_SIZE} Tf {LEFT} {y} Td ({pdf_escape(text)}) Tj ET" for y, text in lines
    ]
    return "\n".join(ops).encode("latin-1")


def widths_entries() -> str:
    first, last = 32, 126
    widths = " ".join(str(HELVETICA_WIDTHS.get(chr(code), 0)) for code in range(first, last + 1))
    return f"/FirstChar {first} /LastChar {last} /Widths [{widths}]"


RICH_TEXT = (
    '<?xml version="1.0"?><body xmlns="http://www.w3.org/1999/xhtml"'
    ' xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"'
    ' xfa:APIVersion="Acrobat:11.0.0" xfa:spec="2.0.2">'
    '<p dir="ltr">Due date &amp; <span style="font-weight:bold">penalty</span></p>'
    "<p>Срок оплаты</p></body>"
)


def build_pdf() -> bytes:
    writer = PdfWriter()
    catalog = writer.reserve()
    pages = writer.reserve()
    info = writer.add(
        b"<< /Title (Text annotations) /Subject (Synthetic fixture: text-bearing annotations)"
        b" /Producer (scripts/make_annotation_fixture.py) >>"
    )
    font = writer.add(
        (
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica"
            f" /Encoding /WinAnsiEncoding {widths_entries()} >>"
        ).encode("latin-1")
    )
    page1 = writer.reserve()
    page2 = writer.reserve()

    line1, line2, line3 = PAGE1_LINES
    thirty = phrase_box(line1[1], line1[0], "thirty days")
    shall = phrase_box(line1[1], line1[0], "shall")
    payment = phrase_box(line2[1], line2[0], "Payment is due")
    invoice = phrase_box(line2[1], line2[0], "invoice")
    highlight = phrase_box(line3[1], line3[0], "highlight")

    def add(entries: str) -> int:
        return writer.add(f"<< /Type /Annot {entries} >>".encode("latin-1"))

    a1 = writer.reserve()
    p1 = add(f"/Subtype /Popup /Rect [580 600 760 700] /Parent {a1} 0 R /Open false")
    writer.set(
        a1,
        (
            f"<< /Type /Annot /Subtype /Text /Rect [40 694 58 712] /P {page1} 0 R"
            " /Name /Comment /C [1 0.8 0] /T (Alice)"
            " /M (D:20261118093000+07'00') /CreationDate (D:20261118090000+07'00')"
            f" /Contents (Check the delivery term.) /Popup {p1} 0 R >>"
        ).encode("latin-1"),
    )
    free_text_appearance = writer.stream(
        f"/Type /XObject /Subtype /Form /BBox [0 0 180 30] /Resources << /Font << /F1 {font} 0 R >> >>",
        b"BT /F1 10 Tf 4 11 Td (Draft for review) Tj ET",
    )
    a2 = add(
        f"/Subtype /FreeText /Rect [360 600 540 630] /P {page1} 0 R /T (Bob)"
        " /M (D:20261118094500+07'00') /Contents (Draft for review) /DA (/Helv 10 Tf 0 g)"
        f" /AP << /N {free_text_appearance} 0 R >>"
    )
    a3 = writer.reserve()
    p3 = add(f"/Subtype /Popup /Rect [580 500 760 600] /Parent {a3} 0 R /Open false")
    writer.set(
        a3,
        (
            f"<< /Type /Annot /Subtype /Highlight /Rect {rect(thirty)} /P {page1} 0 R"
            f" /QuadPoints {quad_points(thirty)} /C [1 1 0] /T (Alice)"
            " /CreationDate (D:20261118091500Z)"
            f" /Contents (Should this be 30 business days?) /Popup {p3} 0 R >>"
        ).encode("latin-1"),
    )
    a4 = add(
        f"/Subtype /Text /Rect [540 680 558 698] /P {page1} 0 R /IRT {a3} 0 R"
        f" /T {text_string('Борис')} /M (D:20261118100000+03'00')"
        f" /Contents {text_string('Да, рабочих дней.')}"
    )
    a5 = add(
        f"/Subtype /Text /Rect [40 694 58 712] /P {page1} 0 R /IRT {a1} 0 R /F 30"
        " /State /Completed /StateModel /Review /T (Alice)"
        " /M (D:20261118110000+07'00') /Contents (Completed set by Alice)"
    )
    a6 = add(
        f"/Subtype /Highlight /Rect {rect(highlight)} /P {page1} 0 R"
        f" /QuadPoints {quad_points(highlight)} /C [1 1 0] /T (Alice)"
    )
    a7 = add(
        f"/Subtype /Underline /Rect {rect(payment)} /P {page1} 0 R"
        f" /QuadPoints {quad_points(payment)} /C [0 0 1] /T {text_string('Ирина')}"
        f" /M (D:20261118120000+03'00') /RC {text_string(RICH_TEXT)}"
    )
    a8 = add(
        f"/Subtype /Highlight /Rect (not a rectangle) /P {page1} 0 R"
        " /T (Alice) /Contents (Malformed rectangle)"
    )
    a9 = add(
        f"/Subtype /StrikeOut /Rect {rect(shall)} /P {page1} 0 R"
        f" /QuadPoints {quad_points(shall)} /C [1 0 0] /T (Bob)"
        " /M (D:20261118095000+07'00') /Contents (Delete)"
    )
    a10 = add(
        f"/Subtype /Squiggly /Rect {rect(invoice)} /P {page1} 0 R"
        f" /QuadPoints {quad_points(invoice)} /C [0 1 0] /T (Bob)"
        " /M (D:20261118095500+07'00') /Contents (Wrong word?)"
    )
    not_a_dictionary = writer.add(b"42")
    page1_annots = [a1, p1, a2, a3, p3, a4, a5, a6, a7, a8, a9, a10, not_a_dictionary]

    b1 = add(
        f"/Subtype /Text /Rect [40 694 58 712] /P {page2} 0 R /T (Bob)"
        " /M (D:20261118130000+07'00') /Contents (Second page note)"
    )
    b2 = add(
        f"/Subtype /Text /Rect [40 694 58 712] /P {page2} 0 R /IRT {b1} 0 R /F 30"
        " /State /Accepted /StateModel /Review /T (Alice)"
        " /M (D:20261118131000+07'00') /Contents (Accepted set by Alice)"
    )
    b3 = add(
        f"/Subtype /Text /Rect [40 694 58 712] /P {page2} 0 R /IRT {b1} 0 R /F 30"
        " /State /Rejected /StateModel /Review /T (Alice)"
        " /M (D:20261118132000+07'00') /Contents (Rejected set by Alice)"
    )
    page2_annots = [b1, b2, b3]

    resources = f"/Resources << /Font << /F1 {font} 0 R >> >>"
    contents1 = writer.stream("", content_stream(PAGE1_LINES))
    annots1 = " ".join(f"{ref} 0 R" for ref in page1_annots)
    writer.set(
        page1,
        (
            f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 612 792]"
            f" /CropBox [36 36 576 756] {resources} /Contents {contents1} 0 R"
            f" /Annots [{annots1}] >>"
        ).encode("latin-1"),
    )
    contents2 = writer.stream("", content_stream(PAGE2_LINES))
    annots2 = " ".join(f"{ref} 0 R" for ref in page2_annots)
    writer.set(
        page2,
        (
            f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 612 792]"
            f" {resources} /Contents {contents2} 0 R /Annots [{annots2}] >>"
        ).encode("latin-1"),
    )
    writer.set(pages, f"<< /Type /Pages /Kids [{page1} 0 R {page2} 0 R] /Count 2 >>".encode("latin-1"))
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
