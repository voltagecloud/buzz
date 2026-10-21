# Buzz's own copy of Codex, apart from any `codex` the user installed.
# The host runs it as `bash -c` (see `DECLARED` in processes.rs), each time
# with one of these steps:
#
#   run [ARGS...]                  runs the copy in use with ARGS
#   install VERSION TARGET SHA256  downloads a release and checks it
#   use VERSION                    makes a downloaded release the one in use
set -eu

# Shared by every Buzz build on this computer, so there is one download.
if [ "$(uname)" = Darwin ]; then
  dir="$HOME/Library/Application Support/Buzz/codex"
else
  dir="${XDG_DATA_HOME:-$HOME/.local/share}/buzz/codex"
fi

# Versions name folders, so only digits and dots: no `..` or `/` escapes.
valid() {
  case "$1" in "" | [!0-9]* | *[!0-9.]*) return 1 ;; esac
}

# Whether release $1 is newer than release $2.
newer() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    split(a, x, "."); split(b, y, ".")
    for (i = 1; i <= 3; i++) if (x[i] + 0 != y[i] + 0) exit !(x[i] + 0 > y[i] + 0)
    exit 1
  }'
}

command="$1"
shift
case "$command" in
run)
  # A failed exec returns (execfail, without errexit), so the launcher can
  # look again when a release it read was pruned before it started.
  set +e
  shopt -s execfail
  while :; do
    # The plugin names a release to test it before it is in use.
    version="${BUZZ_CODEX_VERSION:-$(cat "$dir/current" 2>/dev/null)}"
    # 127, as for a missing program: the plugin reads it as "not installed".
    valid "$version" || { echo "Codex is not installed" >&2; exit 127; }
    exec "$dir/$version/bin/codex" "$@"
    # Builds that switched after the read above may have pruned its release;
    # then start the one `current` names now.
    [ -z "${BUZZ_CODEX_VERSION:-}" ] &&
      [ "$(cat "$dir/current" 2>/dev/null)" != "$version" ] || exit 127
  done
  ;;
install)
  version="$1" target="$2" checksum="$3"
  valid "$version" || { echo "Invalid version" >&2; exit 2; }
  case "$target" in
  aarch64-apple-darwin | x86_64-apple-darwin | \
    aarch64-unknown-linux-musl | x86_64-unknown-linux-musl) ;;
  *) echo "Unsupported platform" >&2; exit 2 ;;
  esac
  # Each download unpacks in its own hidden folder, so two Buzz builds
  # updating at once do not write over each other, and a stopped one leaves
  # nothing.
  part="$dir/.install.$$"
  mkdir -p "$part/package"
  trap 'rm -rf "$part"' EXIT
  trap 'exit 143' INT TERM
  # Give up on a stalled download rather than holding the update forever.
  curl -fsSL --speed-limit 1024 --speed-time 60 -o "$part/package.tar.gz" \
    "https://releases.openai.com/codex/releases/$version/codex-package-$target.tar.gz"
  # The release's published checksum, which the plugin passes in.
  actual=$( (shasum -a 256 "$part/package.tar.gz" 2>/dev/null ||
    sha256sum "$part/package.tar.gz") | cut -d" " -f1)
  [ "$actual" = "$checksum" ] || { echo "The download did not match its checksum" >&2; exit 1; }
  tar -xzf "$part/package.tar.gz" -C "$part/package"
  test -x "$part/package/bin/codex"
  # A rename, so a session starting now sees no folder or the whole release.
  # Another build that already installed this release keeps its folder.
  [ -d "$dir/$version" ] || mv "$part/package" "$dir/$version"
  ;;
use)
  version="$1"
  valid "$version" || { echo "Invalid version" >&2; exit 2; }
  # One switch at a time across Buzz builds, so none prunes the release
  # another just switched to. The kernel holds the lock for this process and
  # releases it when the process ends, however it ends.
  exec 9>>"$dir/.lock"
  if command -v flock >/dev/null; then
    flock 9
  else # macOS has no flock(1)
    perl -MFcntl=:flock -e 'open(my $lock, ">&=9") or die "$!\n";
      flock($lock, LOCK_EX) or die "$!\n"'
  fi
  # Checked under the lock: another build may have pruned it meanwhile.
  test -x "$dir/$version/bin/codex" ||
    { echo "Codex $version is no longer downloaded" >&2; exit 1; }
  previous=$(cat "$dir/current" 2>/dev/null || true)
  printf %s "$version" >"$dir/current.$$"
  mv -f "$dir/current.$$" "$dir/current"
  # Keep the new release, the one before it to fall back to, and any newer
  # one, which another build may be testing; remove the rest.
  for folder in "$dir"/*/; do
    folder=$(basename "$folder")
    [ "$folder" = "$version" ] || [ "$folder" = "$previous" ] ||
      newer "$folder" "$version" || rm -rf "${dir:?}/$folder"
  done
  ;;
*)
  echo "Unknown command: $command" >&2
  exit 2
  ;;
esac
