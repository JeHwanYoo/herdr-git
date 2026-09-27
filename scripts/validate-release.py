import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


def validate_release():
    cargo = tomllib.loads(Path("Cargo.toml").read_text())
    plugin = tomllib.loads(Path("herdr-plugin.toml").read_text())
    lock = tomllib.loads(Path("Cargo.lock").read_text())
    version = cargo["package"]["version"]
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
        raise ValueError("The release version must be major.minor.patch")
    package = next(
        p for p in lock["package"]
        if p["name"] == cargo["package"]["name"] and "source" not in p
    )
    if plugin["version"] != version or package["version"] != version:
        raise ValueError("Cargo.toml, Cargo.lock, and herdr-plugin.toml versions must match")
    tags = subprocess.check_output(["git", "tag", "--list"], text=True).splitlines()
    released = [tuple(map(int, tag[1:].split("."))) for tag in tags if re.fullmatch(r"v\d+\.\d+\.\d+", tag)]
    if released and tuple(map(int, version.split("."))) <= max(released):
        raise ValueError("The release version must be newer than existing release tags")
    notes_path = Path(f"changelogs/{version}.md")
    if not notes_path.exists():
        raise ValueError(f"Write {notes_path} before releasing")
    heading, _, body = notes_path.read_text().partition("\n")
    if not re.fullmatch(rf"# {re.escape(version)}(?: — \d{{4}}-\d{{2}}-\d{{2}})?", heading) or not body.strip():
        raise ValueError(f"{notes_path} needs a matching version heading and a nonempty body")
    entry = f"- [{version}](changelogs/{version}.md)"
    if entry not in Path("CHANGELOG.md").read_text().splitlines():
        raise ValueError(f"Add {entry} to CHANGELOG.md")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a") as stream:
            stream.write(f"version={version}\n")
    print(f"Ready to release v{version}")


if __name__ == "__main__":
    try:
        validate_release()
    except (ValueError, StopIteration) as error:
        sys.exit(str(error))
