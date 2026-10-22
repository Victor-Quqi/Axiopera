"""Check release tags and package native binaries. Requires Python 3.11+."""

import argparse
import json
from pathlib import Path
import subprocess
import tarfile
import tomllib
import zipfile


ROOT = Path(__file__).resolve().parent.parent
CODEX_PLUGIN = ROOT / "plugins" / "axiopera" / ".codex-plugin" / "plugin.json"


def version():
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["package"]["version"]


def check_tag(tag):
    expected = f"v{version()}"
    if tag != expected:
        raise SystemExit(f"Tag {tag!r} does not match Cargo.toml version {expected!r}")
    plugin = json.loads(CODEX_PLUGIN.read_text(encoding="utf-8")).get("version")
    if plugin != version():
        raise SystemExit(f"Codex plugin version {plugin!r} does not match Cargo.toml version {version()!r}")


def package(target, binary, output):
    name = "axiopera.exe" if "windows" in target else "axiopera"
    binary = (binary or ROOT / "target" / target / "release" / name).resolve()
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    expected = f"axiopera {version()}"
    if actual != expected:
        raise SystemExit(f"Binary reports {actual!r}; expected {expected!r}")

    stem = f"axiopera-v{version()}-{target}"
    members = [(binary, name)]
    for relative in ("LICENSE", "README.md", "README.zh-CN.md", "src/guide.md"):
        members.append((ROOT / relative, relative))
    members.extend((p, p.relative_to(ROOT).as_posix()) for p in sorted((ROOT / "docs").glob("*.md")))
    output.mkdir(parents=True, exist_ok=True)
    if "windows" in target:
        archive = output / f"{stem}.zip"
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as packed:
            for path, relative in members:
                packed.write(path, f"{stem}/{relative}")
    else:
        archive = output / f"{stem}.tar.gz"
        with tarfile.open(archive, "w:gz") as packed:
            for path, relative in members:
                info = packed.gettarinfo(str(path), arcname=f"{stem}/{relative}")
                info.mode = 0o755 if relative == name else 0o644
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                with path.open("rb") as content:
                    packed.addfile(info, content)
    print(archive)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    tag = commands.add_parser("check-tag")
    tag.add_argument("tag")
    archive = commands.add_parser("package")
    archive.add_argument("target")
    archive.add_argument("--binary", type=Path)
    archive.add_argument("--output", type=Path, default=ROOT / "target" / "release-assets")
    args = parser.parse_args()
    if args.command == "check-tag":
        check_tag(args.tag)
    else:
        package(args.target, args.binary, args.output)


if __name__ == "__main__":
    main()
