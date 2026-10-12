import unittest
from pathlib import Path

from PIL import Image, ImageDraw


class LocatorTests(unittest.TestCase):
    def test_color_target_ignores_thin_border_and_clipped_edge(self):
        self.assertTrue(Path(__file__).with_name('computer_control.py').exists(), 'GUI control missing')
        import computer_control
        self.assertTrue(hasattr(computer_control, 'locate_color'), 'GUI target locator missing')
        image = Image.new('RGB', (300, 200))
        draw = ImageDraw.Draw(image)
        draw.rectangle((0, 0, 299, 199), outline=(244, 162, 97), width=2)
        draw.rectangle((40, 50, 87, 97), fill=(244, 162, 97))
        self.assertEqual(computer_control.locate_color(image, (244, 162, 97), 30, 100), (63, 73))
        self.assertIsNone(computer_control.locate_color(image, (255, 255, 255), 30, 100))


if __name__ == '__main__':
    unittest.main()
