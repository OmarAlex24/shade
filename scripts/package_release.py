#!/usr/bin/env python3
"""Package an accepted Shade binary without rebuilding or installing it."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import tarfile
import tempfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(value):
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def repo_file(repo, name):
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or "\\" in name or any(
        ord(char) < 32 for char in name
    ):
        raise ValueError(f"Unsafe repository path: {name!r}")
    target = (repo / path).resolve()
    if not target.is_relative_to(repo.resolve()):
        raise ValueError(f"Repository path escapes the checkout: {name!r}")
    return target.read_bytes()


def verified_file(repo, name, expected):
    data = repo_file(repo, name)
    if digest(data) != expected:
        raise ValueError(f"Acceptance digest mismatch: {name}")
    return data


def accepted_payload(repo, binary):
    evidence_name = "artifacts/v1-release-validation.json"
    evidence_bytes = repo_file(repo, evidence_name)
    evidence = json.loads(evidence_bytes)
    if (
        evidence["schema_version"] != 1
        or evidence["status"] != "passed"
        or evidence["final_v1_release"] is not True
    ):
        raise ValueError("V1 acceptance is not complete")
    if evidence["checks"]["performance"]["passed"] is not True:
        raise ValueError("Performance acceptance is not complete")

    sources = evidence["source_files"]
    if not sources or len({item["path"] for item in sources}) != len(sources):
        raise ValueError("Invalid accepted source inventory")
    encoded = json.dumps(sources, sort_keys=True, separators=(",", ":")).encode()
    if digest(encoded) != evidence["source_sha256"]:
        raise ValueError("Accepted source inventory digest mismatch")
    for item in sources:
        verified_file(repo, item["path"], item["sha256"])

    binary_bytes = binary.read_bytes()
    if digest(binary_bytes) != evidence["binary_sha256"]:
        raise ValueError("Binary does not match the accepted distribution")
    payload = {"shade": binary_bytes, evidence_name: evidence_bytes}
    for name, expected in evidence["files"].items():
        if not name.startswith("artifacts/v1-release/"):
            raise ValueError(f"Unexpected evidence path: {name!r}")
        payload[name] = verified_file(repo, name, expected)

    performance_path = evidence["checks"]["performance"]["artifact"]
    performance = json.loads(payload[performance_path])
    environment = performance["environment"]
    if (
        performance["schema_version"] != 1
        or performance["status"] != "passed"
        or performance["failures"]
        or performance["mode"] != "release"
        or performance["thresholds_enforced"] is not True
        or environment["target_os"] != "macos"
        or environment["target_arch"] != "aarch64"
        or environment["filesystem"] != "apfs"
        or environment["shade_binary_sha256"] != evidence["binary_sha256"]
        or environment["gate_binary_sha256"] != evidence["gate_binary_sha256"]
        or performance["configuration"]
        != {"entries": 25000, "workspaces": 20, "latency_samples": 100, "payload_bytes": 1024}
        or not performance["materialization"]["latency"]["p50_ms"] < 300
        or not performance["materialization"]["latency"]["p95_ms"] < 1000
        or not performance["context"]["latency"]["p95_ms"] < 200
        or not performance["cli_ipc"]["latency"]["p95_ms"] < 10
        or not performance["common_response_budget"]["max_bytes"] <= 512
        # A context for a session that is not `active` carries the `lifecycle`
        # field the common case elides, so it is held to the same budget plus
        # that bounded suffix rather than being left unmeasured.
        or not performance["common_response_budget"]["max_lifecycle_bytes"] <= 544
        or performance["space"]["within_threshold"] is not True
    ):
        raise ValueError("The recorded APFS release gate does not certify this binary")

    version = environment["shade"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("Invalid release version")
    skill = evidence["checks"]["skill"]
    if skill["passed"] is not True or not skill["tokens"] <= 350:
        raise ValueError("Skill acceptance is not complete")
    skill_path = "skills/shade-workspaces/SKILL.md"
    payload[skill_path] = verified_file(repo, skill_path, skill["sha256"])
    payload["LICENSE"] = repo_file(repo, "LICENSE")
    payload["INSTALL.md"] = f"""# Shade {version}

This package contains the accepted Apple Silicon macOS binary. Shade requires
APFS, Git and the host package managers declared by each project. It never
installs runtimes or falls back to full copies.

From this extracted directory, verify every packaged file and install:

```sh
shasum -a 256 -c SHA256SUMS
./shade --version
./shade install
```

Installation copies this executable to
`~/Library/Application Support/Shade/bin/shade` and starts the per-user
`com.shade.daemon` LaunchAgent. Add the binary directory to your PATH, then run
`shade doctor`. The installer returns after the private socket answers.

`MANIFEST.json` identifies the accepted binary, source inventory and evidence.
`artifacts/v1-release-validation.json` and its referenced artifacts preserve
functional, recovery and performance acceptance, including the earlier failed
performance measurement. Timings apply to the recorded workload and host.
Checksums verify file integrity; this package is not signed or notarized.

The compact operating skill is at `skills/shade-workspaces/SKILL.md`.
Rust and TypeScript SDK source remains in the Shade repository.
""".encode()
    manifest = {
        "schema_version": 1,
        "version": version,
        "target": "aarch64-apple-darwin",
        "binary_sha256": evidence["binary_sha256"],
        "source_sha256": evidence["source_sha256"],
        "acceptance_sha256": digest(evidence_bytes),
        "files": {name: digest(data) for name, data in sorted(payload.items())},
    }
    payload["MANIFEST.json"] = json_bytes(manifest)
    payload["SHA256SUMS"] = "".join(
        f"{digest(data)}  {name}\n" for name, data in sorted(payload.items())
    ).encode()
    return f"shade-{version}-aarch64-apple-darwin", payload


def archive_bytes(name, payload):
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            for relative, data in sorted(payload.items()):
                member = tarfile.TarInfo(f"{name}/{relative}")
                member.size = len(data)
                member.mode = 0o755 if relative == "shade" else 0o644
                member.mtime = member.uid = member.gid = 0
                archive.addfile(member, io.BytesIO(data))
    return output.getvalue()


def package_release(repo, binary, output_dir):
    name, payload = accepted_payload(repo, binary)
    archive = archive_bytes(name, payload)
    checksum = digest(archive)
    outputs = {
        f"{name}.tar.gz": archive,
        f"{name}.tar.gz.sha256": f"{checksum}  {name}.tar.gz\n".encode(),
    }
    output_dir.mkdir(parents=True, exist_ok=True)
    for filename in outputs:
        if os.path.lexists(output_dir / filename):
            raise FileExistsError(f"Output already exists: {output_dir / filename}")
    published = []
    try:
        with tempfile.TemporaryDirectory(prefix=".shade-package-", dir=output_dir) as staging:
            for filename, data in outputs.items():
                staged = Path(staging) / filename
                staged.write_bytes(data)
                os.link(staged, output_dir / filename)
                published.append(output_dir / filename)
    except OSError:
        for path in published:
            path.unlink()
        raise
    return {
        "status": "packaged",
        "archive": str((output_dir / f"{name}.tar.gz").resolve()),
        "sha256": checksum,
        "binary_sha256": digest(payload["shade"]),
        "files": len(payload),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="Already accepted release binary")
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    args = parser.parse_args()
    try:
        result = package_release(Path(__file__).resolve().parents[1], args.binary, args.output_dir)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"Packaging failed: {error}\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
