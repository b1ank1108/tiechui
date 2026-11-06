"""Skipped SDK cases must bypass session and scripted-reply assertions."""
from contextlib import redirect_stdout
import io
from pathlib import Path
import unittest
from unittest.mock import MagicMock, patch

import smoke


class HarnessTests(unittest.TestCase):
    def test_sdk_skips_are_successful_and_cleanup_the_fixture(self):
        for missing_repository in (True, False):
            with self.subTest(missing_repository=missing_repository):
                fixtures = [MagicMock(sessions=[]), MagicMock(sessions=[])]
                cases = ["sdk-rust", "sdk-typescript"] if missing_repository else ["sdk-typescript"]
                output = io.StringIO()
                with patch.object(smoke, "PLUGINS", Path(__file__).parent), \
                     patch.object(Path, "is_dir", return_value=not missing_repository), \
                     patch.object(smoke.shutil, "which", return_value=None), \
                     patch.object(smoke, "Fixture", side_effect=fixtures), \
                     patch.object(smoke.sys, "argv", ["smoke.py", *cases]), \
                     redirect_stdout(output):
                    self.assertEqual(smoke.main(), 0)
                for case, fixture in zip(cases, fixtures):
                    self.assertIn("SKIP " + case, output.getvalue())
                    fixture.close.assert_called_once_with(False)
                self.assertNotIn("PASS", output.getvalue())
                self.assertNotIn("FAIL", output.getvalue())
