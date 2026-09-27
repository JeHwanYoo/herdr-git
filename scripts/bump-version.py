from pathlib import Path
import re
import sys
import tomllib


def bump_version(kind):
    if kind not in {"patch", "minor", "major"}:
        raise ValueError("Choose patch, minor, or major")
    cargo_path = Path("Cargo.toml")
    plugin_path = Path("herdr-plugin.toml")
    lock_path = Path("Cargo.lock")
    cargo = tomllib.loads(cargo_path.read_text())
    plugin = tomllib.loads(plugin_path.read_text())
    lock = tomllib.loads(lock_path.read_text())
    current = cargo["package"]["version"]
    name = cargo["package"]["name"]
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", current):
        raise ValueError("The current version must be major.minor.patch")
    package = next(p for p in lock["package"] if p["name"] == name and "source" not in p)
    if plugin["version"] != current or package["version"] != current:
        raise ValueError("Cargo.toml, Cargo.lock, and herdr-plugin.toml versions must match")
    major, minor, patch = map(int, current.split("."))
    if kind == "major":
        major, minor, patch = major + 1, 0, 0
    elif kind == "minor":
        minor, patch = minor + 1, 0
    else:
        patch += 1
    version = f"{major}.{minor}.{patch}"
    updates = {}
    for path in (cargo_path, plugin_path):
        text, count = re.subn(
            rf'^version = "{re.escape(current)}"$', f'version = "{version}"',
            path.read_text(), count=1, flags=re.MULTILINE,
        )
        if count != 1:
            raise ValueError(f"Could not update version in {path}")
        updates[path] = text
    pattern = rf'(\[\[package\]\]\nname = "{re.escape(name)}"\nversion = "){re.escape(current)}(")'
    text, count = re.subn(pattern, lambda m: f"{m[1]}{version}{m[2]}", lock_path.read_text())
    if count != 1:
        raise ValueError("Could not update package version in Cargo.lock")
    updates[lock_path] = text
    for path, text in updates.items():
        path.write_text(text)
    print(f"{current} → {version}")
    print(f"Write changelogs/{version}.md and add its link to CHANGELOG.md before releasing.")


if __name__ == "__main__":
    try:
        bump_version(sys.argv[1])
    except (ValueError, IndexError, StopIteration) as error:
        sys.exit(str(error))
