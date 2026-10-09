#!/usr/bin/env python3
"""Explicit macOS pilot test; never launch or mutate /Applications/ImageOptim.app.

Requires an already-built zb and ImageMagick. Network is permitted only for zb's
checksum-pinned download. The vendor app runs with network and general writes
denied, isolated preferences, one generated image, and narrow worker-temp access.
Evidence remains in the printed run-owned directory for inspection.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tempfile
import zlib


def run(args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, **kwargs)


def tree_digest(root):
    digest = hashlib.sha256()
    for path in [root, *sorted(root.rglob("*"))]:
        digest.update(str(path.relative_to(root)).encode())
        digest.update(str(path.lstat().st_mode).encode())
        if path.is_symlink():
            digest.update(os.readlink(path).encode())
        elif path.is_file():
            digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("--zb", type=Path, required=True)
    parser.add_argument("--archive", type=Path, help="Optional local archive; CLI still verifies the pinned SHA")
    args = parser.parse_args()
    if os.uname().sysname != "Darwin":
        raise SystemExit("This smoke test requires macOS")
    magick = shutil.which("magick")
    if not magick:
        raise SystemExit("Existing ImageMagick is required for independent pixel verification")
    run_dir = Path(tempfile.mkdtemp(prefix="zerobrew-final-", dir="/tmp")).resolve()
    print(f"Evidence: {run_dir}", flush=True)
    apps, home, temporary = [run_dir / name for name in ("apps", "home", "tmp")]
    for directory in (apps, home, temporary, home / ".Trash"):
        directory.mkdir()
    manifest = Path(__file__).resolve().parents[1] / "zb_core/fixtures/cask_imageoptim.json"
    production = Path("/Applications/ImageOptim.app")
    production_before = tree_digest(production) if production.exists() else None
    env = os.environ.copy()
    for key in ("ZEROBREW_ROOT", "ZEROBREW_PREFIX", "ZEROBREW_AUTO_INIT"):
        env.pop(key, None)
    zb = [args.zb.resolve(), "--appdir", apps]
    install = [*zb, "--app-manifest", manifest]
    if args.archive:
        install += ["--app-archive", args.archive.resolve()]
    install += ["install", "cask:imageoptim"]
    first = run(install, env=env).stdout.decode()
    repeated = run(install, env=env).stdout.decode()
    assert "already installed and unchanged" in repeated
    receipt = json.loads(run([*zb, "info", "cask:imageoptim"], env=env).stdout)
    assert receipt["bundle"]["identifier"] == "net.pornel.ImageOptim"
    assert receipt["bundle"]["version"] == "1.9.3"
    assert json.loads(run([*zb, "--app-manifest", manifest, "outdated", "--json"], env=env).stdout) == []
    app = apps / "ImageOptim.app"
    run(["/usr/bin/codesign", "--verify", "--deep", "--strict", app])
    signing = run(["/usr/bin/codesign", "-dv", "--verbose=4", app]).stderr.decode()
    assert "TeamIdentifier=59KZTZA4XR" in signing
    bundle_before = tree_digest(app)

    # Deliberately uncompressed synthetic RGB PNG. Only its copy is optimized.
    width, height = 47, 43
    raw = b"".join(b"\x00" + b"\x29\x83\xc1" * width for _ in range(height))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, level=0)) + chunk(b"IEND", b"")
    source, image = run_dir / "source.png", run_dir / "optimized.png"
    source.write_bytes(png)
    image.write_bytes(png)
    pixels_before = run([magick, source, "-depth", "8", "RGBA:-"]).stdout
    worker_dir = Path(run(["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"]).stdout.decode().strip()).resolve()
    # SBPL regex syntax: character classes avoid ambiguous backslash escaping.
    escaped = re.escape(str(worker_dir)).replace(r"\.", "[.]")
    worker_pattern = escaped + "/ImageOptim[.][A-Za-z]+Worker[.][0-9a-f]+[.][0-9a-f]+[.]temp$"
    sandbox = run_dir / "smoke.sb"
    sandbox.write_text('(version 1)\n(allow default)\n(deny network*)\n(deny file-write*)\n'
                       f'(allow file-write* (subpath "{run_dir}"))\n'
                       '(allow file-write* (literal "/dev/null"))\n'
                       '(deny mach-lookup (global-name "com.apple.cfprefsd.daemon") (global-name "com.apple.cfprefsd.agent"))\n'
                       f'(allow file-write* (regex #"^{worker_pattern}"))\n')
    sandbox_cmd = ["/usr/bin/sandbox-exec", "-f", sandbox]
    run([*sandbox_cmd, "/usr/bin/touch", run_dir / "allowed"])
    forbidden = worker_dir / (run_dir.name + "-forbidden")
    denied = subprocess.run([str(a) for a in [*sandbox_cmd, "/usr/bin/touch", forbidden]], capture_output=True)
    assert denied.returncode != 0 and not forbidden.exists()
    app_env = dict(env, HOME=str(home), CFFIXED_USER_HOME=str(home), TMPDIR=str(temporary))
    smoke = run([*sandbox_cmd, app / "Contents/MacOS/ImageOptim", image], env=app_env)
    (run_dir / "smoke.log").write_bytes(smoke.stdout + smoke.stderr)
    pixels_after = run([magick, image, "-depth", "8", "RGBA:-"]).stdout
    assert pixels_before == pixels_after
    assert len(pixels_after) == width * height * 4
    assert image.stat().st_size < len(png)
    assert source.read_bytes() == png
    assert tree_digest(app) == bundle_before
    # The final CLI verifies its own receipt again after the app ran.
    assert json.loads(run([*zb, "info", "cask:imageoptim"], env=env).stdout) == receipt
    run([*zb, "uninstall", "cask:imageoptim"], env=env)
    assert not app.exists() and source.read_bytes() == png
    production_after = tree_digest(production) if production.exists() else None
    assert production_before == production_after
    report = {"host": os.uname().nodename, "receipt": receipt, "signature": signing,
              "install": first.strip(), "repeat": repeated.strip(), "input_bytes": len(png),
              "output_bytes": image.stat().st_size, "pixel_sha256": hashlib.sha256(pixels_after).hexdigest(),
              "pixel_equal": True, "source_unchanged": True, "production_bundle_unchanged": True,
              "uninstall_verified": True, "network_denied_for_app": True}
    (run_dir / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: report[k] for k in ("input_bytes", "output_bytes", "pixel_equal", "production_bundle_unchanged", "uninstall_verified")}, indent=2))


if __name__ == "__main__":
    main()
