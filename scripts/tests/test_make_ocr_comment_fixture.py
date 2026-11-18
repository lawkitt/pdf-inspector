import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from make_ocr_comment_fixture import (
    BODY_LINES,
    DOT,
    FIXTURE,
    GLYPHS,
    IMAGE_HEIGHT,
    IMAGE_WIDTH,
    LEFT_PX,
    body_image,
    build_pdf,
    pixel_box,
)


class OcrCommentFixtureTests(unittest.TestCase):
    def test_build_reproduces_the_committed_fixture(self):
        self.assertEqual(build_pdf(), FIXTURE.read_bytes())

    def test_every_body_character_has_a_five_by_seven_glyph(self):
        for _, text in BODY_LINES:
            for char in text:
                rows = GLYPHS[char]
                self.assertEqual(len(rows), 7, char)
                self.assertTrue(all(len(row) == 5 for row in rows), char)

    def test_body_image_is_gray_letter_page_with_black_text(self):
        image = body_image()
        self.assertEqual(len(image), IMAGE_WIDTH * IMAGE_HEIGHT)
        self.assertEqual(set(image), {0, 255})
        top, text = BODY_LINES[0]
        # The first glyph's top-left dot ("S" starts at its second column).
        self.assertEqual(image[top * IMAGE_WIDTH + LEFT_PX + DOT], 0)
        self.assertEqual(image[(top - 1) * IMAGE_WIDTH + LEFT_PX + DOT], 255)

    def test_pixel_box_spans_the_characters_in_pdf_points(self):
        x0, y0, x1, y1 = pixel_box(100, "AB", 0, 1)
        self.assertAlmostEqual(x0, (LEFT_PX - 2) * 0.72)
        self.assertAlmostEqual(x1, (LEFT_PX + 5 * DOT + 2) * 0.72)
        self.assertAlmostEqual(y1 - y0, (7 * DOT + 4) * 0.72)


if __name__ == "__main__":
    unittest.main()
