#!/usr/bin/env python3
"""Validate the RustFS compatibility contract and build CI reports."""

import argparse
import json
from pathlib import Path


ALLOWED_EXPECTATIONS = {"supported", "not-implemented", "version-dependent"}
REQUIRED_SURFACES = {"s3", "admin-v3", "admin-v4", "streaming"}


def validate_manifest(manifest):
    if manifest.get("schema_version") != 1:
        raise ValueError("schema_version must be 1")

    servers = manifest.get("servers", [])
    versions = [server.get("version") for server in servers]
    if len(servers) != 2 or len(set(versions)) != len(versions):
        raise ValueError("servers must contain two unique releases")
    if manifest.get("supported_floor") not in versions:
        raise ValueError("supported_floor must identify a matrix server")
    if manifest.get("target") not in versions:
        raise ValueError("target must identify a matrix server")

    for server in servers:
        image = server.get("image", "")
        expected_suffix = f":{server['version']}"
        if image.endswith(":latest") or not image.endswith(expected_suffix):
            raise ValueError(f"server image must be pinned to {server['version']}")

    capabilities = manifest.get("capabilities", [])
    identifiers = [capability.get("id") for capability in capabilities]
    if len(identifiers) != len(set(identifiers)):
        raise ValueError("capability identifiers must be unique")
    surfaces = {capability.get("surface") for capability in capabilities}
    missing_surfaces = REQUIRED_SURFACES - surfaces
    if missing_surfaces:
        raise ValueError(f"missing capability surfaces: {sorted(missing_surfaces)}")

    for capability in capabilities:
        expectations = capability.get("expectations", {})
        if set(expectations) != set(versions):
            raise ValueError(f"{capability['id']} must classify every matrix server")
        invalid = set(expectations.values()) - ALLOWED_EXPECTATIONS
        if invalid:
            raise ValueError(f"{capability['id']} has invalid expectations: {sorted(invalid)}")

    batch = next(
        (capability for capability in capabilities if capability.get("id") == "batch-jobs"),
        None,
    )
    target = manifest["target"]
    if batch is None or batch["expectations"].get(target) != "not-implemented":
        raise ValueError("batch-jobs must remain not-implemented on the target release")


def validate_repository(root, manifest=None):
    integration_paths = [
        root / ".github/workflows/integration.yml",
        root / "docker/docker-compose.yml",
        root / "crates/cli/tests/integration.rs",
    ]
    for path in integration_paths:
        if "rustfs/rustfs:latest" in path.read_text(encoding="utf-8"):
            raise ValueError(f"integration path uses an unpinned RustFS image: {path}")

    if manifest is not None:
        workflow = integration_paths[0].read_text(encoding="utf-8")
        for server in manifest["servers"]:
            if server["image"] not in workflow:
                raise ValueError(f"workflow is missing matrix server image: {server['image']}")
        compose = integration_paths[1].read_text(encoding="utf-8")
        target_image = next(
            server["image"]
            for server in manifest["servers"]
            if server["version"] == manifest["target"]
        )
        if target_image not in compose:
            raise ValueError(f"Compose default does not use target image: {target_image}")


def parse_probes(path):
    probes = {}
    if path is None:
        return probes
    for line in path.read_text(encoding="utf-8").splitlines():
        identifier, result, detail = line.split("\t", maxsplit=2)
        probes[identifier] = {"result": result, "detail": detail}
    return probes


def build_report(manifest, version, probes, image_id):
    server = next(
        (candidate for candidate in manifest["servers"] if candidate["version"] == version),
        None,
    )
    if server is None:
        raise ValueError(f"version is not in the compatibility matrix: {version}")

    capabilities = []
    for capability in manifest["capabilities"]:
        probe = probes.get(
            capability["id"],
            {"result": "not-run", "detail": "No probe result was recorded"},
        )
        capabilities.append(
            {
                "id": capability["id"],
                "surface": capability["surface"],
                "expected": capability["expectations"][version],
                "result": probe["result"],
                "detail": probe["detail"],
            }
        )

    return {
        "schema_version": manifest["schema_version"],
        "server": {
            "version": server["version"],
            "image": server["image"],
            "image_id": image_id,
        },
        "capabilities": capabilities,
    }


def write_report(path, report):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--manifest", type=Path, default=Path(".github/rustfs-compatibility.json")
    )
    parser.add_argument("--check-repository", action="store_true")
    parser.add_argument("--report", metavar="VERSION")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--probes", type=Path)
    parser.add_argument("--image-id", default="unknown")
    args = parser.parse_args()

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    validate_manifest(manifest)
    if args.check_repository:
        validate_repository(Path(__file__).resolve().parents[1], manifest)
    if args.report:
        if args.output is None:
            parser.error("--output is required with --report")
        write_report(
            args.output,
            build_report(manifest, args.report, parse_probes(args.probes), args.image_id),
        )


if __name__ == "__main__":
    main()
