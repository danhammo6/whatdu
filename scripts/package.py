"""Package a native release build; run from the repository root with Python 3.11+."""

import argparse
import hashlib
from pathlib import Path
import tarfile
import tomllib
import zipfile


TARGETS = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    args = parser.parse_args()
    version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    name = f"whatdu-{version}-{args.target}"
    windows = "windows" in args.target
    binary = Path("target") / args.target / "release" / (
        "whatdu.exe" if windows else "whatdu"
    )
    files = [binary, Path("README.md"), Path("LICENSE")]
    for path in files:
        if not path.is_file():
            parser.error(f"missing release input: {path}")
    output = Path("dist")
    output.mkdir(exist_ok=True)
    archive = output / (name + (".zip" if windows else ".tar.gz"))
    if windows:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as package:
            for path in files:
                package.write(path, arcname=f"{name}/{path.name}")
    else:
        with tarfile.open(archive, "w:gz") as package:
            for path in files:
                package.add(path, arcname=f"{name}/{path.name}")
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8"
    )
    print(archive)


if __name__ == "__main__":
    main()
