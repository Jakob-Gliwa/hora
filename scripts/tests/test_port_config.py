import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest

SCRIPT = Path(__file__).resolve().parent.parent / "port-config.py"
spec = importlib.util.spec_from_file_location("port_config", SCRIPT)
port = importlib.util.module_from_spec(spec)
spec.loader.exec_module(port)


class ClassesTests(unittest.TestCase):
    def test_complete_hundreds_become_classes(self):
        self.assertEqual(port.classes(range(200, 400)), '["2xx", "3xx"]')
        self.assertEqual(port.classes(range(200, 300)), '"2xx"')

    def test_single_codes_and_mixtures(self):
        self.assertEqual(port.classes([200]), "200")
        self.assertEqual(port.classes([401, 200]), "[200, 401]")
        self.assertEqual(port.classes([*range(200, 300), 401]), '["2xx", 401]')
        self.assertEqual(port.classes([*range(200, 299)]), "[" + ", ".join(map(str, range(200, 299))) + "]")

    def test_garbage_is_refused(self):
        for bad in ([600], ["200"], [99]):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                port.classes(bad)


class ConvertTests(unittest.TestCase):
    SOURCE = (
        "# private\n"
        "[[monitors]]\n"
        "id = \"a\"\n"
        "target = \"${HORA_A_URL}\"\n"
        f"expected_statuses = [{', '.join(map(str, range(200, 400)))}]\n"
        "[[monitors]]\n"
        "id = \"b\"\n"
        "  expected_statuses = [200]  # only OK\n"
        "[[monitors]]\n"
        "id = \"c\"\n"
        "expected_statuses = [\n  200,\n  401,\n]\n"
        "[[monitors]]\n"
        "id = \"d\"\n"
        "expected_status = 204\n"
    )

    def test_renames_and_keeps_everything_else(self):
        converted, changes = port.convert(self.SOURCE)
        self.assertEqual(changes, {"expected_statuses -> expected_status": 3})
        self.assertNotIn("expected_statuses", converted)
        self.assertIn("# private\n", converted)
        self.assertIn('target = "${HORA_A_URL}"\n', converted)
        self.assertIn("  expected_status = 200  # only OK\n", converted)
        monitors = tomllib.loads(converted)["monitors"]
        self.assertEqual(
            [m.get("expected_status") for m in monitors],
            [["2xx", "3xx"], 200, [200, 401], 204],
        )

    def test_nothing_to_rename_is_the_same_text(self):
        text = "[page]\n[server]\n"
        self.assertEqual(port.convert(text), (text, {}))

    def test_cli_writes_the_output_and_prints_no_values(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "in.toml"
            target = Path(directory) / "out.toml"
            source.write_text(self.SOURCE, encoding="utf-8")
            run = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(source), "-o", str(target)],
                capture_output=True, text=True, check=True)
            self.assertEqual(run.stdout, "")
            self.assertEqual(run.stderr, "expected_statuses -> expected_status: 3\n")
            self.assertNotIn("expected_statuses", target.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
