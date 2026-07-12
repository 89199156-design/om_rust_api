import unittest
import tempfile
from pathlib import Path

from om_downloader.locking import file_lock


class LockingTests(unittest.TestCase):
    def test_file_lock_rejects_reentrant_acquire_and_removes_lock_on_exit(self):
        with tempfile.TemporaryDirectory() as tmp:
            lock_path = Path(tmp) / "locks" / "gfs025.lock"

            with file_lock(lock_path):
                self.assertTrue(lock_path.exists())
                with self.assertRaises(RuntimeError) as ctx:
                    with file_lock(lock_path):
                        pass
                self.assertIn("already running", str(ctx.exception))

            self.assertFalse(lock_path.exists())


if __name__ == "__main__":
    unittest.main()
