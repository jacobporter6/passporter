"""Generator tests use temporary files only; never start herdr or a deployment."""
import argparse
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

spec = importlib.util.spec_from_file_location("create", Path(__file__).with_name("create.py"))
create = importlib.util.module_from_spec(spec)
spec.loader.exec_module(create)


class DeploymentTest(unittest.TestCase):
    def test_isolated_session_with_private_companion_and_quoted_paths(self):
        with tempfile.TemporaryDirectory(prefix="pp-") as temporary:
            base = Path(temporary)
            binary = base / "fake bridge"
            binary.write_text("#!/bin/sh\nexit 99\n")
            binary.chmod(0o700)
            static = base / "web"
            static.mkdir()
            (static / "index.html").write_text("synthetic test build")
            args = argparse.Namespace(name="test", output=base / "session one", herdr_socket=base / "named.sock",
                                      bridge_bin=binary, static_dir=static, bridge_port=18443, ntfy_port=12586,
                                      contact="mailto:ops@example.com")
            root = create.create(args)
            config = tomllib.loads((root / "config.toml").read_text())
            self.assertEqual(config["herdr_socket"], str(args.herdr_socket))
            self.assertEqual(config["push"]["native_ntfy"], "127.0.0.1:12586")
            self.assertEqual(config["push"]["subject"], "mailto:ops@example.com")
            self.assertEqual(config["listen"], "127.0.0.1:18443")
            self.assertEqual((root / "config.toml").stat().st_mode & 0o777, 0o600)
            self.assertEqual(root.stat().st_mode & 0o777, 0o700)
            compose = (root / "compose.yml").read_text()
            self.assertIn(create.NTFY_IMAGE, compose)
            self.assertRegex(create.NTFY_IMAGE, r"@sha256:[0-9a-f]{64}$")
            self.assertNotIn("ports:", compose)
            self.assertIn("127.0.0.1:12586", compose)
            self.assertIn("--visitor-request-limit-exempt-hosts=127.0.0.1", compose)
            self.assertIn("--visitor-subscription-limit=64", compose)
            subprocess.run(["bash", "-n", str(root / "run.sh")], check=True)
            with self.assertRaises(FileExistsError):
                create.create(args)
            args.output = base / "other"
            args.ntfy_port = args.bridge_port
            with self.assertRaises(ValueError):
                create.create(args)
            args.ntfy_port = 12587
            args.contact = "ops@example.com"
            with self.assertRaises(ValueError):
                create.create(args)
            self.assertFalse(args.output.exists(), "nothing written for a refused deployment")


if __name__ == "__main__":
    unittest.main()
