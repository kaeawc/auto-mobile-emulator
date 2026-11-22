"""Resolve an SDK package to a download from Google's repository manifests (no sdkmanager/Java).

usage: python3 -I sdk-resolve.py <manifest.xml> <manifest-base-url> <package-path> [host-os] [host-arch]
prints: url size sha1 revision   (newest stable archive usable on this host)
"""
import sys
import xml.etree.ElementTree as ET

manifest, base_url, path = sys.argv[1:4]
host_os = sys.argv[4] if len(sys.argv) > 4 else "macosx"
host_arch = sys.argv[5] if len(sys.argv) > 5 else "aarch64"
root = ET.parse(manifest).getroot()
channels = {c.get("id"): c.text for c in root.findall("channel")}


def revision(pkg):
    r = pkg.find("revision")
    return (0,) if r is None else tuple(int(r.findtext(k) or 0) for k in ("major", "minor", "micro"))


hits = []
for pkg in root.findall("remotePackage"):
    if pkg.get("path") != path:
        continue
    ref = pkg.find("channelRef")
    if ref is not None and channels.get(ref.get("ref")) != "stable":
        continue
    for a in pkg.findall("archives/archive"):
        if a.findtext("host-os") not in (None, host_os) or a.findtext("host-arch") not in (None, host_arch):
            continue
        c = a.find("complete")
        hits.append((revision(pkg), base_url + c.findtext("url"), c.findtext("size"), c.findtext("checksum")))

if not hits:
    sys.exit(f"no stable archive for {path} on {host_os}/{host_arch}")
best = max(hits)
print(best[1], best[2], best[3], ".".join(map(str, best[0])))
