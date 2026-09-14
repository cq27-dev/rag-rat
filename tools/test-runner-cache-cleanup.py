"""Exercise cleanup's destructive boundary with a fake sweeper and real file locks."""
import fcntl
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("runner-cache-cleanup.sh")


class CleanupTests(unittest.TestCase):
    def setUp(self):
        # Match the production path guard without changing HOME or touching a real runner.
        self.scratch = tempfile.TemporaryDirectory(prefix="actions-runner-cleanup-test-", dir=Path.home())
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.target = self.root / "cargo-target"
        self.profile = self.target / "debug"
        self.profile.mkdir(parents=True)
        self.marker = self.target / "swept"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("cargo", "cargo-sweep"):
            command = self.bin / name
            command.write_text('#!/bin/sh\n[ "$1" = sweep ] || exit 99\n[ "$2" = --maxsize ] || exit 98\n[ "$3" = 20GB ] || exit 97\ntouch "$CARGO_TARGET_DIR/swept"\nexit "${SWEEP_EXIT:-0}"\n')
            command.chmod(0o755)
        self.env = dict(os.environ, CARGO_TARGET_DIR=str(self.target), CARGO_HOME=str(self.root))

    def run_cleanup(self, **env):
        return subprocess.run(["bash", str(SCRIPT)], env=dict(self.env, **env), capture_output=True, text=True, timeout=10)

    def test_sweeps_only_the_configured_target(self):
        sibling = self.root / "sibling"
        sibling.mkdir()
        result = self.run_cleanup()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(self.marker.exists())
        self.assertEqual(list(sibling.iterdir()), [])

    def test_each_cargo_lock_blocks_eviction(self):
        for name in (".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"):
            with self.subTest(name=name), (self.profile / name).open("w") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                result = self.run_cleanup()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("::warning::", result.stdout)
                self.assertFalse(self.marker.exists())

    def test_refuses_unexpected_or_symlinked_targets(self):
        external = self.root / "external"
        external.mkdir()
        result = self.run_cleanup(CARGO_TARGET_DIR=str(external))
        self.assertNotEqual(result.returncode, 0)
        self.profile.rmdir()
        self.target.rmdir()
        self.target.symlink_to(external, target_is_directory=True)
        result = self.run_cleanup()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(external.iterdir()), [])

    def test_sweeper_errors_fail_the_step(self):
        result = self.run_cleanup(SWEEP_EXIT="42")
        self.assertEqual(result.returncode, 42)
        self.assertIn("::error::", result.stdout)


if __name__ == "__main__":
    unittest.main()
