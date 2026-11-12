#!/usr/bin/env python3
"""Generates flatpak-builder sources for all crates in Cargo.lock.

Flatpak builds run without network access, so every crate is downloaded by
flatpak-builder and vendored into $CARGO_HOME/vendor.  Run this again whenever
Cargo.lock changes:

    python3 flatpak/generate-cargo-sources.py Cargo.lock > flatpak/cargo-sources.json
"""

import json
import sys
import tomllib

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"

CARGO_CONFIG = """[source.vendored-sources]
directory = "cargo/vendor"

[source.crates-io]
replace-with = "vendored-sources"
"""


def main(lock_path: str) -> None:
    with open(lock_path, "rb") as f:
        lock = tomllib.load(f)

    sources = []
    for pkg in lock.get("package", []):
        source = pkg.get("source")
        if source is None:
            continue  # the application itself
        if source != REGISTRY:
            sys.exit(f"Unsupported source for {pkg['name']}: {source}")
        name, version, checksum = pkg["name"], pkg["version"], pkg["checksum"]
        dest = f"cargo/vendor/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )
    sources.append({"type": "inline", "contents": CARGO_CONFIG, "dest": "cargo", "dest-filename": "config.toml"})
    json.dump(sources, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "Cargo.lock")
