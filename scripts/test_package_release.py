import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

from package_release import digest, json_bytes, package_release


class PackageReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="shade-package-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "accepted-binary"
        self.binary.write_bytes(b"accepted binary fixture\n")
        self.write("crates/example.rs", b"accepted source\n")
        self.write("LICENSE", b"MIT fixture\n")
        self.write("skills/shade-workspaces/SKILL.md", b"accepted skill\n")
        self.gate = {
            "schema_version": 1,
            "status": "passed",
            "failures": [],
            "mode": "release",
            "thresholds_enforced": True,
            "environment": {
                "target_os": "macos",
                "target_arch": "aarch64",
                "filesystem": "apfs",
                "shade": "0.1.0",
                "shade_binary_sha256": digest(self.binary.read_bytes()),
                "gate_binary_sha256": digest(b"gate fixture"),
            },
            "configuration": {
                "entries": 25000, "workspaces": 20, "latency_samples": 100, "payload_bytes": 1024,
            },
            "materialization": {"latency": {"p50_ms": 270, "p95_ms": 281}},
            "context": {"latency": {"p95_ms": 47}},
            "cli_ipc": {"latency": {"p95_ms": 4}},
            "common_response_budget": {"max_bytes": 512},
            "space": {"within_threshold": True},
        }
        self.gate_path = "artifacts/v1-release/apfs.json"
        self.write(self.gate_path, json_bytes(self.gate))
        self.write("artifacts/v1-release/crash.log", b"accepted recovery evidence fixture\n")
        self.sources = [{"path": "crates/example.rs", "sha256": digest(b"accepted source\n")}]
        self.evidence = {
            "schema_version": 1,
            "status": "passed",
            "final_v1_release": True,
            "source_files": self.sources,
            "source_sha256": self.source_digest(),
            "binary_sha256": digest(self.binary.read_bytes()),
            "gate_binary_sha256": self.gate["environment"]["gate_binary_sha256"],
            "files": {
                name: digest((self.root / name).read_bytes())
                for name in [self.gate_path, "artifacts/v1-release/crash.log"]
            },
            "checks": {
                "performance": {"passed": True, "artifact": self.gate_path},
                "skill": {"passed": True, "tokens": 239, "sha256": digest(b"accepted skill\n")},
            },
        }
        self.save_evidence()

    def write(self, name, data):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def source_digest(self):
        return digest(json.dumps(self.sources, sort_keys=True, separators=(",", ":")).encode())

    def save_evidence(self):
        self.write("artifacts/v1-release-validation.json", json_bytes(self.evidence))

    def package(self, directory="dist"):
        return package_release(self.root, self.binary, self.root / directory)

    def test_reproducible_archive_preserves_binary_permissions_and_checksums(self):
        first = self.package("first")
        second = self.package("second")
        archive_bytes = Path(first["archive"]).read_bytes()
        self.assertEqual(archive_bytes, Path(second["archive"]).read_bytes())
        self.assertEqual(digest(archive_bytes), first["sha256"])
        with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode="r:gz") as archive:
            prefix = "shade-0.1.0-aarch64-apple-darwin/"
            binary = archive.getmember(prefix + "shade")
            self.assertEqual(binary.mode, 0o755)
            self.assertEqual(archive.extractfile(binary).read(), self.binary.read_bytes())
            for line in archive.extractfile(prefix + "SHA256SUMS").read().decode().splitlines():
                expected, name = line.split("  ", 1)
                self.assertEqual(digest(archive.extractfile(prefix + name).read()), expected)

    def test_rejects_modified_binary_before_creating_output(self):
        self.binary.write_bytes(b"different build\n")
        with self.assertRaisesRegex(ValueError, "Binary does not match"):
            self.package()
        self.assertFalse((self.root / "dist").exists())

    def test_rejects_modified_source(self):
        self.write("crates/example.rs", b"unaccepted source\n")
        with self.assertRaisesRegex(ValueError, "Acceptance digest mismatch"):
            self.package()

    def test_rejects_modified_evidence(self):
        self.write("artifacts/v1-release/crash.log", b"changed evidence\n")
        with self.assertRaisesRegex(ValueError, "Acceptance digest mismatch"):
            self.package()

    def test_rejects_modified_skill(self):
        self.write("skills/shade-workspaces/SKILL.md", b"changed skill\n")
        with self.assertRaisesRegex(ValueError, "Acceptance digest mismatch"):
            self.package()

    def test_rejects_incomplete_acceptance(self):
        self.evidence["final_v1_release"] = False
        self.save_evidence()
        with self.assertRaisesRegex(ValueError, "acceptance is not complete"):
            self.package()

    def test_rejects_threshold_miss_despite_passed_label_and_consistent_hashes(self):
        self.gate["materialization"]["latency"]["p50_ms"] = 300
        self.write(self.gate_path, json_bytes(self.gate))
        self.evidence["files"][self.gate_path] = digest(json_bytes(self.gate))
        self.save_evidence()
        with self.assertRaisesRegex(ValueError, "gate does not certify"):
            self.package()

    def test_rejects_paths_outside_checkout(self):
        self.sources[0]["path"] = "../outside.rs"
        self.evidence["source_sha256"] = self.source_digest()
        self.save_evidence()
        with self.assertRaisesRegex(ValueError, "Unsafe repository path"):
            self.package()

    def test_does_not_replace_an_existing_package(self):
        first = self.package()
        original = Path(first["archive"]).read_bytes()
        with self.assertRaises(FileExistsError):
            self.package()
        self.assertEqual(Path(first["archive"]).read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
