#!/usr/bin/env python3
"""Inject the TovekService declaration into an apktool-decoded AndroidManifest.xml.

xbuild's manifest model cannot declare <service> elements or hasCode, so CI
decodes the xbuild APK with apktool, runs this script on the decoded (textual)
AndroidManifest.xml, drops the javac/d8-compiled classes.dex next to it, and
rebuilds. The service runs in its own process so the server survives the UI
being swiped away (see tovek-gui/android/TovekService.java).

Usage: patch_android_manifest.py <path/to/AndroidManifest.xml>
"""

import pathlib
import sys

SERVICE = (
    '        <service'
    ' android:name="dev.tovek.gui.TovekService"'
    ' android:exported="false"'
    ' android:foregroundServiceType="dataSync"'
    ' android:process=":tovek_server"'
    ' android:stopWithTask="false"/>\n'
)


def main() -> None:
    path = pathlib.Path(sys.argv[1])
    text = path.read_text(encoding="utf-8")

    if "TovekService" in text:
        print(f"{path}: already patched")
        return

    # The APK now carries classes.dex with the service class.
    if 'android:hasCode="false"' in text:
        text = text.replace('android:hasCode="false"', 'android:hasCode="true"', 1)
    elif 'android:hasCode="true"' not in text:
        text = text.replace("<application ", '<application android:hasCode="true" ', 1)
    if 'android:hasCode="true"' not in text:
        raise SystemExit(f"{path}: could not force android:hasCode=true")

    closing = "</application>"
    index = text.rindex(closing)
    text = text[:index] + SERVICE + "    " + text[index:]

    path.write_text(text, encoding="utf-8")
    print(f"{path}: injected TovekService (hasCode=true)")


if __name__ == "__main__":
    main()
