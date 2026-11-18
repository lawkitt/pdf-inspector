import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from make_annotation_fixture import (
    FIXTURE,
    build_pdf,
    num,
    phrase_box,
    quad_points,
    text_string,
    text_width,
)


class AnnotationFixtureTests(unittest.TestCase):
    def test_build_reproduces_the_committed_fixture(self):
        self.assertEqual(build_pdf(), FIXTURE.read_bytes())

    def test_text_string_is_a_literal_for_ascii_and_utf16_otherwise(self):
        self.assertEqual(text_string("a (b)"), "(a \\(b\\))")
        self.assertEqual(text_string("Да"), "<FEFF04140430>")

    def test_phrase_box_starts_after_the_preceding_text(self):
        x0, y0, x1, y1 = phrase_box("ab cd", 700, "cd")
        self.assertAlmostEqual(x0, 72 + text_width("ab "))
        self.assertAlmostEqual(x1 - x0, text_width("cd"))
        self.assertEqual((y0, y1), (697, 710))

    def test_quad_points_run_upper_left_upper_right_lower_left_lower_right(self):
        self.assertEqual(quad_points((1, 2, 3, 4)), "[1 4 3 4 1 2 3 2]")

    def test_num_drops_trailing_zeros(self):
        self.assertEqual(num(12.5), "12.5")
        self.assertEqual(num(3.0), "3")
        self.assertEqual(num(-0.0001), "0")


if __name__ == "__main__":
    unittest.main()
