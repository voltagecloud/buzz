"""Copies GStreamer into Buzz.app, so the app plays media on a Mac that
has no GStreamer installed.

Takes GStreamer from the official macOS framework (the
gstreamer-1.0-<version>-universal.pkg runtime). Lays it out as

    Contents/MacOS/buzz-gpui, gst-plugin-scanner
    Contents/Frameworks/<every dylib the above and the plugins load>
    Contents/Resources/lib/gstreamer-1.0/<plugins>
    Contents/Resources/lib/gio/modules/<GIO modules>

which crates/app/src/gst_bundle.rs points GStreamer and GIO at. The
plugins sit under Resources because codesign takes a directory under
PlugIns or Frameworks for a bundle and refuses to sign the app around it. Every copy
is thinned to the one architecture, refers to the others as
@rpath/<name> and has one rpath, to Contents/Frameworks. Then each
Mach-O in the bundle is checked to load only bundled or system libraries.

Usage: python3 scripts/bundle-macos-gstreamer.py <Buzz.app> <prefix> <arch>
(the prefix holds lib/ and libexec/, e.g.
/Library/Frameworks/GStreamer.framework/Versions/1.0).
"""

import os
import pathlib
import shutil
import subprocess
import sys

# What playback, sounds and voice notes use: playbin and its decoders,
# app sources and sinks, audio output and capture, and the MP4 encoder
# path. The same set as scripts/package-windows.py, with Core Audio in
# place of the Windows audio plugins.
PLUGINS = [
    "app", "audioconvert", "audioparsers", "audioresample", "autodetect",
    "coreelements", "osxaudio", "dav1d", "isomp4", "libav", "matroska",
    "mpg123", "ogg", "opus", "playback", "soup", "typefindfunctions",
    "videoconvertscale", "videoparsersbad", "volume", "vorbis", "vpx",
    "wavparse", "flac", "audiotestsrc", "rawparse", "id3demux", "pbtypes",
    "gio", "tcp", "adaptivedemux2", "dash", "hls",
]

# The soup plugins dlopen libsoup by name rather than linking it.
DLOPENED = ["libsoup-3.0.0.dylib"]

SYSTEM = ("/usr/lib/", "/System/")
LOADS = {
    "LC_LOAD_DYLIB", "LC_LOAD_WEAK_DYLIB", "LC_REEXPORT_DYLIB",
    "LC_LOAD_UPWARD_DYLIB", "LC_LAZY_LOAD_DYLIB",
}


def run(*args):
    return subprocess.run(
        [str(a) for a in args], check=True, capture_output=True, text=True
    ).stdout


def load_commands(path):
    """The install name, libraries and rpaths of a one-arch Mach-O."""
    ident, libs, rpaths, cmd = None, [], [], None
    for line in run("otool", "-l", path).splitlines():
        line = line.strip()
        if line.startswith("cmd "):
            cmd = line.split()[1]
        elif line.startswith("name ") or line.startswith("path "):
            value = line[5:].rsplit(" (offset", 1)[0]
            if cmd == "LC_ID_DYLIB":
                ident = value
            elif cmd in LOADS:
                libs.append(value)
            elif cmd == "LC_RPATH":
                rpaths.append(value)
    return ident, libs, rpaths


def is_macho(path):
    with open(path, "rb") as f:
        magic = f.read(4)
    return magic in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe",
                     b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca")


class Bundler:
    def __init__(self, app, prefix, arch):
        self.app = app
        self.prefix = prefix
        self.arch = arch
        self.contents = app / "Contents"
        self.frameworks = self.contents / "Frameworks"
        # name in Frameworks -> the file it came from
        self.libs = {}
        self.pending = []

    def resolve(self, ref, origin, rpaths):
        """The file `ref`, as `origin` (copied from there) refers to it."""
        here = origin.parent
        if ref.startswith("@rpath/"):
            rest = ref[len("@rpath/"):]
            candidates = [self.prefix / "lib" / rest]
            for rpath in rpaths:
                rpath = rpath.replace("@loader_path", str(here))
                rpath = rpath.replace("@executable_path", str(here))
                candidates.append(pathlib.Path(rpath) / rest)
        elif ref.startswith("@loader_path/"):
            candidates = [here / ref[len("@loader_path/"):]]
        elif ref.startswith("@executable_path/"):
            candidates = [here / ref[len("@executable_path/"):]]
        else:
            candidates = [pathlib.Path(ref)]
        for candidate in candidates:
            if candidate.is_file():
                return candidate.resolve()
        sys.exit(f"{origin}: cannot find {ref}")

    def need(self, name, source):
        if name in self.libs:
            if self.libs[name] != source:
                sys.exit(f"two libraries named {name}: {self.libs[name]}, {source}")
            return
        self.libs[name] = source
        self.pending.append((self.frameworks / name, source, "@loader_path"))

    def place(self, dest, source, rpath):
        """Copies `source` to `dest` (unless it is there) and rewires it."""
        if dest != source:
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, dest)
        os.chmod(dest, 0o755)
        archs = run("lipo", "-archs", dest).split()
        if self.arch not in archs:
            sys.exit(f"{source} has no {self.arch} code ({' '.join(archs)})")
        if len(archs) > 1:
            run("lipo", "-thin", self.arch, dest, "-output", dest)
        ident, libs, rpaths = load_commands(dest)
        args = []
        if ident is not None:
            args += ["-id", f"@rpath/{dest.name}"]
        for ref in libs:
            if ref.startswith(SYSTEM):
                continue
            found = self.resolve(ref, source, rpaths)
            name = pathlib.Path(ref).name
            self.need(name, found)
            if ref != f"@rpath/{name}":
                args += ["-change", ref, f"@rpath/{name}"]
        for old in dict.fromkeys(rpaths):
            args += ["-delete_rpath", old]
        if args:
            run("install_name_tool", *args, dest)
        # A path listed twice survives one deletion.
        while (left := load_commands(dest)[2]):
            run("install_name_tool", "-delete_rpath", left[0], dest)
        run("install_name_tool", "-add_rpath", rpath, dest)

    def bundle(self):
        to_frameworks = "@executable_path/../Frameworks"
        exe = self.contents / "MacOS" / "buzz-gpui"
        self.pending.append((exe, exe, to_frameworks))
        scanner = self.prefix / "libexec" / "gstreamer-1.0" / "gst-plugin-scanner"
        self.pending.append((self.contents / "MacOS" / scanner.name, scanner, to_frameworks))
        plugins = self.contents / "Resources" / "lib" / "gstreamer-1.0"
        for name in PLUGINS:
            plugin = self.prefix / "lib" / "gstreamer-1.0" / f"libgst{name}.dylib"
            if not plugin.is_file():
                sys.exit(f"plugin {name} is not in {plugin.parent}")
            self.pending.append((plugins / plugin.name, plugin, "@loader_path/../../../Frameworks"))
        modules = self.prefix / "lib" / "gio" / "modules"
        for module in sorted(modules.glob("*.so")) + sorted(modules.glob("*.dylib")):
            self.pending.append((
                self.contents / "Resources" / "lib" / "gio" / "modules" / module.name,
                module, "@loader_path/../../../../Frameworks"))
        for name in DLOPENED:
            self.need(name, (self.prefix / "lib" / name).resolve())
        while self.pending:
            self.place(*self.pending.pop())
        print(f"bundled {len(PLUGINS)} plugins and {len(self.libs)} libraries")


def check(app):
    """Fails unless every Mach-O in `app` loads only bundled or system
    libraries, through rpaths inside the bundle."""
    app = app.resolve()
    macos = app / "Contents" / "MacOS"
    bad = []
    count = 0
    for path in sorted(p for p in app.rglob("*") if p.is_file() and not p.is_symlink()):
        if not is_macho(path):
            continue
        count += 1
        _, libs, rpaths = load_commands(path)
        dirs = []
        for rpath in rpaths:
            if not rpath.startswith(("@loader_path", "@executable_path")):
                bad.append(f"{path}: rpath {rpath}")
                continue
            rpath = rpath.replace("@loader_path", str(path.parent))
            rpath = rpath.replace("@executable_path", str(macos))
            dirs.append(pathlib.Path(os.path.normpath(rpath)))
        for ref in libs:
            if ref.startswith(SYSTEM):
                continue
            if not ref.startswith("@rpath/"):
                bad.append(f"{path}: loads {ref}")
            else:
                name = ref[len("@rpath/"):]
                found = [d / name for d in dirs if (d / name).is_file()]
                if not found or app not in found[0].resolve().parents:
                    bad.append(f"{path}: {ref} is not in the bundle")
    if bad:
        sys.exit("\n".join(["not self-contained:"] + bad))
    print(f"{count} Mach-O files load only bundled and system libraries")


def main():
    app = pathlib.Path(sys.argv[1]).resolve()
    prefix = pathlib.Path(sys.argv[2]).resolve()
    arch = sys.argv[3]
    Bundler(app, prefix, arch).bundle()
    check(app)


if __name__ == "__main__":
    main()
