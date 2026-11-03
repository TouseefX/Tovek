#!/usr/bin/env python3
"""Package a Tovek GUI build together with the Luau host (client) scripts.

Every platform package — the four desktop builds and the Android APK — carries
both executor-side host scripts next to the app, so each decompile transport's
client ships with each build:

  * decompile.client.luau        (single-script transport, POST /decompile)
  * decompile-batch.client.luau  (batch + raw transport, POST /decompile/batch)

Desktop platforms get `tovek-gui(.exe)` + scripts + licence in a .zip
(Windows) or .tar.gz (Linux/macOS); Android gets the signed APK + scripts in a
.zip. A SHA256SUMS-gui-<platform>.txt is written next to the archive.
"""
import argparse
import hashlib
import pathlib
import shutil
import tarfile
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]

HOST_SCRIPTS = ('decompile.client.luau', 'decompile-batch.client.luau')

DESKTOP_PLATFORMS = ('windows-x86_64', 'linux-x86_64', 'linux-aarch64', 'macos-aarch64')
ANDROID_PLATFORMS = ('android-arm64',)

README = """Tovek GUI {version} — {platform}

Contents
  {binary:<28} the Tovek GUI ({kind})
  decompile.client.luau        executor client, single-script transport (POST /decompile)
  decompile-batch.client.luau  executor client, batch + raw transport (POST /decompile/batch)
  LICENSE.txt

Quick start
  1. Start the app and open the Server tab, then press "Start server"
     (desktop: you can also run the single-file and folder decompilers offline).
  2. Copy one of the client scripts into your executor (the app's
     "Client scripts" tab has copy buttons already retargeted at your port).
  3. Run it in-game: every LocalScript/ModuleScript is decompiled through the
     local server by SynSaveInstance.

Android
  Install the APK, keep the app in the foreground (it holds the screen on and
  a partial wake lock while the server runs) and point an on-device executor
  at http://localhost:3000.
"""


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as handle:
        for block in iter(lambda: handle.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--platform', required=True,
                        choices=DESKTOP_PLATFORMS + ANDROID_PLATFORMS)
    parser.add_argument('--bin-dir', type=pathlib.Path,
                        help='directory holding tovek-gui(.exe) (desktop platforms)')
    parser.add_argument('--apk', type=pathlib.Path,
                        help='signed APK path (android platforms)')
    parser.add_argument('--version', default='V2.7')
    parser.add_argument('--out', type=pathlib.Path, required=True)
    args = parser.parse_args()

    android = args.platform in ANDROID_PLATFORMS
    if android:
        if not args.apk or not args.apk.is_file():
            parser.error('--apk must point at the signed APK for android platforms')
        binary = args.apk
        binary_name = 'tovek-gui.apk'
        kind = 'Android app, NativeActivity'
    else:
        if not args.bin_dir:
            parser.error('--bin-dir is required for desktop platforms')
        suffix = '.exe' if args.platform.startswith('windows') else ''
        binary = args.bin_dir / ('tovek-gui' + suffix)
        if not binary.is_file():
            parser.error(f'missing binary: {binary}')
        binary_name = binary.name
        kind = 'desktop app'

    name = f'tovek-gui-{args.platform}'
    args.out.mkdir(parents=True, exist_ok=True)
    stage = args.out / name
    if stage.exists():
        shutil.rmtree(stage)
    stage.mkdir()

    shutil.copy2(binary, stage / binary_name)
    for script in HOST_SCRIPTS:
        source = ROOT / script
        assert source.is_file(), f'missing host script: {source}'
        shutil.copy2(source, stage / script)
    shutil.copy2(ROOT / 'LICENSE.txt', stage / 'LICENSE.txt')
    (stage / 'README.txt').write_text(README.format(
        version=args.version, platform=args.platform,
        binary=binary_name, kind=kind), encoding='utf-8')

    if args.platform.startswith('windows') or android:
        archive = args.out / f'{name}.zip'
        with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as bundle:
            for path in sorted(stage.rglob('*')):
                bundle.write(path, f'{name}/{path.relative_to(stage)}')
    else:
        archive = args.out / f'{name}.tar.gz'
        with tarfile.open(archive, 'w:gz') as bundle:
            def executable(info):
                if pathlib.Path(info.name).name == binary_name:
                    info.mode = 0o755
                return info
            bundle.add(stage, arcname=name, filter=executable)

    shutil.rmtree(stage)
    checksum = sha256(archive)
    sums = args.out / f'SHA256SUMS-gui-{args.platform}.txt'
    sums.write_text(f'{checksum}  {archive.name}\n', encoding='utf-8')
    print(f'packaged {archive} ({archive.stat().st_size} bytes)')
    print(f'{checksum}  {archive.name}')


if __name__ == '__main__':
    main()
