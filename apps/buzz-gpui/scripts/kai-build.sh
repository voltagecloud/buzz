# Builds buzz-gpui inside `kai build gpui` (see Kaifile.roc at the
# repository root). Kai runs it from the repository root with no network,
# the vendored crates in $KAI_INPUTS/gpui-crates and the buzz-app commit
# they come from in $KAI_INPUTS/buzz-app.
set -eu

root=$PWD
vendor="${TMPDIR:-/tmp}/buzz-gpui-crates"

# buzz-app's plugin-manager crate (buzzodz-plugins) reads files outside its
# own directory: its build script includes ../build_env.rs and its catalog
# embeds each bundled plugin's manifest from the repository's src/bundled.
# Vendoring keeps only the crate, so give it copies of both, as the flake's
# overrideVendorGitCheckout does. The vendor directory is read-only, so
# patch a copy.
cp -rL "$KAI_INPUTS/gpui-crates" "$vendor"
chmod -R u+w "$vendor"
plugins=$(echo "$vendor"/buzzodz-plugins-*)
test -d "$plugins"
cp "$KAI_INPUTS/buzz-app/crates/build_env.rs" "$plugins/build_env.rs"
sed -i 's|#\[path = "../build_env.rs"\]|#[path = "build_env.rs"]|' "$plugins/build.rs"
for manifest in "$KAI_INPUTS/buzz-app"/src/bundled/*/manifest.json; do
	name=$(basename "$(dirname "$manifest")")
	mkdir -p "$plugins/src/bundled/$name"
	cp "$manifest" "$plugins/src/bundled/$name/manifest.json"
done
sed -i 's|"../../../src/bundled/|"bundled/|g' "$plugins/src/lib.rs"
# Cargo checks a vendored crate's files against this list; a git crate has
# no published checksum, so an empty list lets the patched files through.
printf '{"files":{},"package":null}' > "$plugins/.cargo-checksum.json"

export CARGO_HOME="${TMPDIR:-/tmp}/cargo-home"
export RUSTFLAGS="--remap-path-prefix=$vendor=/crates"
cd apps/buzz-gpui
cargo build --release --offline --locked -p buzz-gpui \
	--config "$vendor/.cargo/config.toml"

mkdir -p "$root/buzz-gpui-out/bin"
cp target/release/buzz-gpui "$root/buzz-gpui-out/bin/"
