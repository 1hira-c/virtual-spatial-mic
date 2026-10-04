"""Development-only dependency inventory and notice generation from Cargo.lock.

Follows normal dependencies of the shipped Rust product, using Cargo's platform
filter. Includes proc-macro dependencies conservatively; excludes test/build-only
edges. Registry sources remain local and their absolute paths are never emitted.
"""
import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PRODUCT = "vsm-studio"
TARGET = "x86_64-pc-windows-msvc"
# Licenses whose terms would extend to the combined executable.
STRONG_COPYLEFT = ("GPL-", "AGPL-", "LGPL-", "SSPL-", "EUPL-", "OSL-")


def metadata(metadata_file: Path | None = None):
    if metadata_file:
        return json.loads(metadata_file.read_text(encoding="utf-8-sig"))
    return json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1",
        "--filter-platform", TARGET], cwd=ROOT, encoding="utf-8"))


def shipped_packages(meta, name=PRODUCT):
    """Registry packages reachable from `name` through normal dependencies."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    pending = [next(p["id"] for p in packages.values() if p["name"] == name)]
    visited = set()
    while pending:
        key = pending.pop()
        if key in visited:
            continue
        visited.add(key)
        pending.extend(d["pkg"] for d in nodes[key]["deps"]
                       if any(k["kind"] is None for k in d["dep_kinds"]))
    return [packages[k] for k in sorted(visited, key=lambda k: (packages[k]["name"], packages[k]["version"]))
            if packages[k]["source"] is not None]


def alternatives(expression):
    """SPDX alternatives as term lists; parentheses are flattened conservatively."""
    flat = expression.replace("(", " ").replace(")", " ").replace("/", " OR ")
    return [[t for t in alt.replace(" WITH ", " AND ").split(" AND ") if t.strip()]
            for alt in flat.split(" OR ")]


def is_mpl(expression):
    return all(any(t.strip().startswith("MPL-") for t in alt) for alt in alternatives(expression))


def check(package):
    expression = package.get("license")
    if not expression:
        raise ValueError(f"{package['name']} {package['version']} declares no SPDX license")
    usable = [alt for alt in alternatives(expression)
              if not any(t.strip().startswith(STRONG_COPYLEFT) for t in alt)]
    if not usable:
        raise ValueError(f"{package['name']} {package['version']} is only available under {expression}")


def generate(destination: Path, metadata_file: Path | None = None):
    destination.mkdir(parents=True, exist_ok=True)
    inventory, sections = [], []
    for package in shipped_packages(metadata(metadata_file)):
        check(package)
        manifest = Path(package["manifest_path"]).parent
        files = set()
        for pattern in ["LICENSE*", "COPYING*", "NOTICE*", "license*", "licence*", "licenses/*"]:
            files.update(p for p in manifest.glob(pattern) if p.is_file())
        if package.get("license_file"):
            files.add(manifest / package["license_file"])
        row = {k: package.get(k) for k in ["name", "version", "license", "repository"]}
        row["license_files"] = [p.relative_to(manifest).as_posix() for p in sorted(files)]
        inventory.append(row)
        sections.append(f"\n{'='*72}\n{row['name']} {row['version']}\nSPDX: {row['license']}\n{row['repository'] or ''}\n")
        for path in sorted(files):
            if path.stat().st_size > 2 * 1024 * 1024:
                raise ValueError(f"License file too large: {package['name']}")
            sections.append(f"\n--- {path.relative_to(manifest).as_posix()} ---\n" + path.read_text(encoding="utf-8", errors="replace"))
        if not files:
            sections.append("\nNo separate license file in the registry package. See the SPDX declaration and upstream repository above.\n")
    (destination / "rust-NOTICES.txt").write_text("".join(sections), encoding="utf-8", newline="\n")
    (destination / "rust-dependencies.json").write_text(json.dumps({
        "target": TARGET, "source": "Cargo.lock", "product": PRODUCT, "crates": inventory,
        "mpl_crates": [f"{r['name']} {r['version']}" for r in inventory if is_mpl(r["license"])],
        "note": "Third-party crates only; does not assign a license to Virtual Spatial Mic."},
        ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("destination", type=Path)
    parser.add_argument("--metadata", type=Path)
    args = parser.parse_args()
    generate(args.destination, args.metadata)
