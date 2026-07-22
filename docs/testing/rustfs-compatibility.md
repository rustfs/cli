# RustFS compatibility baseline

The integration suite uses explicit RustFS releases so a new server image cannot change CLI behavior without review. The supported floor is `1.0.0-beta.9`, and the current target is `1.0.0-beta.10`.

The machine-readable contract is `.github/rustfs-compatibility.json`. It records each server image and classifies the S3 data plane, Admin API v3, Admin API v4, streaming notifications, and deliberate server stubs. CI validates the contract, runs smoke tests against both releases, and publishes one `rustfs-compatibility-<version>` artifact per matrix entry.

## Run locally

Validate the contract without Docker:

```bash
python3 -m unittest scripts/tests/test_rustfs_compatibility.py
python3 scripts/rustfs_compatibility.py --check-repository
```

Start the target release:

```bash
docker compose -f docker/docker-compose.yml up -d
```

Start the supported floor:

```bash
RUSTFS_IMAGE=rustfs/rustfs:1.0.0-beta.9 \
  docker compose -f docker/docker-compose.yml up -d
```

## Update the baseline

1. Add the new semantic image tag to `.github/rustfs-compatibility.json`; never use a moving tag.
2. Update every capability expectation from RustFS release notes and route-level behavior. A registered route is not sufficient evidence that a capability is supported.
3. Keep a negative probe for every deliberate `NotImplemented` response. In beta.10, batch job start is the baseline negative probe.
4. Update the smoke matrix and Compose default to match the manifest target.
5. Run the contract checks and the full integration workflow before dropping an older release.

An expectation of `version-dependent` means the probe is intentionally not required on that release. It must not be interpreted as supported.
