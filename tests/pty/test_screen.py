"""Guard the emulator behavior that makes PTY assertions meaningful."""
import unittest

from smoke import Screen


class ScreenTests(unittest.TestCase):
    def test_synchronized_updates_mark_incomplete_frames(self):
        screen = Screen(10, 3)
        screen.feed(b'\x1b[?2026hpartial')
        self.assertTrue(screen.updating)
        screen.feed(b'\x1b[?2026l')
        self.assertFalse(screen.updating)

    def test_incremental_queries_and_unicode_cells(self):
        screen = Screen(12, 3)
        data = '\x1b[?u\x1b]10;?\x07\x1b]11;?\x1b\\中a\uf243'.encode()
        for byte in data:
            screen.feed(bytes([byte]))
        self.assertEqual(screen.queries, {"keyboard", "10", "11"})
        self.assertEqual(len(screen.replies), 3)
        self.assertEqual(screen.cells[0][:4], ["中", "", "a", "\uf243"])

    def test_erase_display_and_scrollback_do_not_leave_stale_matches(self):
        screen = Screen(10, 3)
        screen.feed(b'first\r\nsecond\r\nthird\x1b[2;3H\x1b[J')
        self.assertEqual(screen.text(), 'first\nse\n')
        screen.feed(b'\x1b[1J')
        self.assertEqual(screen.text(), '\n\n')
        screen.feed(b'alive\x1b[3J')
        self.assertIn('alive', screen.text())
        screen.feed(b'\x1b[2J')
        self.assertEqual(screen.text(), '\n\n')
        self.assertEqual((screen.row, screen.col), (1, 7))

    def test_erase_line_movement_and_alternate_restore(self):
        screen = Screen(10, 3)
        screen.feed(b'main\x1b[?1049hother\x1b[2D\x1b[K')
        self.assertEqual(screen.text(), 'oth\n\n')
        screen.feed(b'\x1b[1K')
        self.assertEqual(screen.text(), '\n\n')
        screen.feed(b'\x1b[?1049l!')
        self.assertEqual(screen.text(), 'main!\n\n')
        screen.feed(b'\x1b[2K')
        self.assertEqual(screen.text(), '\n\n')

    def test_scrolling_wrapping_and_resize(self):
        screen = Screen(4, 2)
        screen.feed(b'abcd\x1b[0me\r\nlast')
        self.assertEqual(screen.text(), 'e\nlast')
        screen.resize(6, 3)
        self.assertEqual(screen.text(), 'e\nlast\n')


if __name__ == '__main__':
    unittest.main()
