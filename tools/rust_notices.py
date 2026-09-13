"""Development-only dependency inventory and notice generation from Cargo.lock.

Follows normal dependencies for each shipped Rust target, using Cargo's platform
filter. Includes proc-macro dependencies conservatively; excludes test/build-only
edges. Registry sources remain local and their absolute paths are never emitted.
"""
import argparse
import json
from pathlib import Path
import subprocess

def generate(destination: Path, metadata_file: Path | None = None):
    root = Path(__file__).resolve().parents[1]
    if metadata_file:
        metadata = json.loads(metadata_file.read_text(encoding="utf-8-sig"))
    else:
        metadata = json.loads(subprocess.check_output([
            "cargo", "metadata", "--locked", "--format-version", "1",
            "--filter-platform", "x86_64-pc-windows-msvc"], cwd=root, encoding="utf-8"))
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    destination.mkdir(parents=True, exist_ok=True)
    inventories = {}
    for name, filename in [("vsm-studio", "studio"), ("vsm-obs", "obs")]:
        pending = [next(p["id"] for p in packages.values() if p["name"] == name)]
        visited = set()
        while pending:
            key = pending.pop()
            if key in visited:
                continue
            visited.add(key)
            pending.extend(d["pkg"] for d in nodes[key]["deps"]
                           if any(k["kind"] is None for k in d["dep_kinds"]))
        inventory, sections = [], []
        for key in sorted(visited, key=lambda k: (packages[k]["name"], packages[k]["version"])):
            package = packages[key]
            if package["source"] is None:
                continue
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
        inventories[filename] = inventory
        (destination / f"rust-{filename}-NOTICES.txt").write_text("".join(sections), encoding="utf-8", newline="\n")
    (destination / "rust-dependencies.json").write_text(json.dumps({
        "target": "x86_64-pc-windows-msvc", "source": "Cargo.lock", "products": inventories,
        "note": "Third-party crates only; does not assign a license to Virtual Spatial Mic."}, ensure_ascii=False, indent=2)+"\n", encoding="utf-8", newline="\n")

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("destination", type=Path)
    parser.add_argument("--metadata", type=Path)
    args = parser.parse_args()
    generate(args.destination, args.metadata)
