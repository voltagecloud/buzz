# Buzz's development environment for Kai (https://github.com/thebrandonlucas/kai),
# beside Hermit (bin/hermit.hcl), which it does not replace. Kai runs on Linux
# only and needs Nix with flakes.
#
#   kai update                      # pin sources and downloads in .kai/lock.json
#   kai shell                       # the toolchain Hermit pins
#   kai shell mobile                # plus Flutter
#   kai shell gpui                  # plus the libraries buzz-gpui links
#   kai build gpui                  # buzz-gpui, built offline in Kai's sandbox
app [kaifile] {
	pf: platform "https://github.com/thebrandonlucas/kai/releases/download/v0.0.11/65bk6qdahqxn1mm6LR6Hkyw8dSdoeRZGwjNSuiPfvguP.tar.zst",
	std: "https://github.com/thebrandonlucas/kai/releases/download/v0.0.11/std-6Jw5GxNCFkpnvBbnFmQg4KAVEn5bAfEveSZfDSzTDs42.tar.zst",
}

import std.Std

import "rust-toolchain.toml" as toolchain : Str
import "apps/buzz-gpui/Cargo.lock" as gpui_lock : Str

# A static binary from a GitHub release, at the version bin/hermit.hcl pins.
# `kai update` records each file's SHA-256 in .kai/lock.json.
release = |name, files|
	FetchLocked(
		name,
		files.map(|(system, url, program)| { system, url, mirrors: [], program }),
	)

github = "https://github.com"

pinned = [
	release(
		"just",
		[
			("x86_64-linux", "${github}/casey/just/releases/download/1.46.0/just-1.46.0-x86_64-unknown-linux-musl.tar.gz", Archive("just")),
			("aarch64-linux", "${github}/casey/just/releases/download/1.46.0/just-1.46.0-aarch64-unknown-linux-musl.tar.gz", Archive("just")),
		],
	),
	release(
		"biome",
		[
			("x86_64-linux", "${github}/biomejs/biome/releases/download/%40biomejs%2Fbiome%402.4.7/biome-linux-x64-musl", File),
			("aarch64-linux", "${github}/biomejs/biome/releases/download/%40biomejs%2Fbiome%402.4.7/biome-linux-arm64-musl", File),
		],
	),
	release(
		"cargo-deny",
		[
			("x86_64-linux", "${github}/EmbarkStudios/cargo-deny/releases/download/0.19.0/cargo-deny-0.19.0-x86_64-unknown-linux-musl.tar.gz", Archive("cargo-deny-0.19.0-x86_64-unknown-linux-musl/cargo-deny")),
			("aarch64-linux", "${github}/EmbarkStudios/cargo-deny/releases/download/0.19.0/cargo-deny-0.19.0-aarch64-unknown-linux-musl.tar.gz", Archive("cargo-deny-0.19.0-aarch64-unknown-linux-musl/cargo-deny")),
		],
	),
	release(
		"uv",
		[
			("x86_64-linux", "${github}/astral-sh/uv/releases/download/0.12.21/uv-x86_64-unknown-linux-musl.tar.gz", Archive("uv-x86_64-unknown-linux-musl/uv")),
			("aarch64-linux", "${github}/astral-sh/uv/releases/download/0.12.21/uv-aarch64-unknown-linux-musl.tar.gz", Archive("uv-aarch64-unknown-linux-musl/uv")),
		],
	),
	# One program per download, so uvx is its own pin of the same archive.
	release(
		"uvx",
		[
			("x86_64-linux", "${github}/astral-sh/uv/releases/download/0.12.21/uv-x86_64-unknown-linux-musl.tar.gz", Archive("uv-x86_64-unknown-linux-musl/uvx")),
			("aarch64-linux", "${github}/astral-sh/uv/releases/download/0.12.21/uv-aarch64-unknown-linux-musl.tar.gz", Archive("uv-aarch64-unknown-linux-musl/uvx")),
		],
	),
	release(
		"lefthook",
		[
			("x86_64-linux", "${github}/evilmartians/lefthook/releases/download/v2.1.3/lefthook_2.1.3_Linux_x86_64", File),
			("aarch64-linux", "${github}/evilmartians/lefthook/releases/download/v2.1.3/lefthook_2.1.3_Linux_aarch64", File),
		],
	),
	release(
		"pgschema",
		[
			("x86_64-linux", "${github}/pgplex/pgschema/releases/download/v1.7.4/pgschema-1.7.4-linux-amd64", File),
			("aarch64-linux", "${github}/pgplex/pgschema/releases/download/v1.7.4/pgschema-1.7.4-linux-arm64", File),
		],
	),
]

# The libraries buzz-gpui links and loads, as its flake.nix lists them.
gpui_libraries = [
	"freetype",
	"openssl",
	"dbus",
	"sqlite",
	"zstd",
	"wayland",
	"vulkan-loader",
	"vulkan-headers",
	"libxcb",
	"libxkbcommon",
	"libx11",
	"libxcursor",
	"libxi",
	"fontconfig",
	"alsa-lib",
	"gst_all_1.gstreamer",
	"gst_all_1.gst-plugins-base",
	"gst_all_1.gst-plugins-good",
	"gst_all_1.gst-libav",
]

gpui_runtime_libraries = [
	"freetype",
	"openssl",
	"dbus",
	"sqlite",
	"zstd",
	"wayland",
	"vulkan-loader",
	"libxcb",
	"libxkbcommon",
	"libx11",
	"libxcursor",
	"libxi",
	"fontconfig",
	"alsa-lib",
]

kaifile = Std.kaifile(
	[
		Name("buzz"),
		Systems(["x86_64-linux", "aarch64-linux"]),
		Overlay("rust", "github:oxalica/rust-overlay"),
		Follows("rust", "nixpkgs", "default"),
		# Hermit's toolchain. Rust comes from rust-toolchain.toml; the tools
		# above at Hermit's versions; the rest from nixpkgs, at the versions
		# the locked nixpkgs has.
		Environment(
			"dev",
			[
				RustToolchain(toolchain),
				Overlays(["rust"]),
				Tools([
					"just#just",
					"biome#biome",
					"cargo-deny#cargo-deny",
					"uv#uv",
					"uvx#uvx",
					"lefthook#lefthook",
					"pgschema#pgschema",
					"actionlint",
					"nodejs_24",
					"pnpm_11",
					"cmake",
					# Hermit leaves these to the machine; the root workspace's
					# native crates need them.
					"pkg-config",
					"openssl",
					"protobuf",
					"perl",
				]),
			],
		),
		Environment("mobile", [Extend("dev"), Tools(["flutter"])]),
		Environment(
			"gpui",
			[
				RustToolchain(toolchain),
				Overlays(["rust"]),
				Tools(["pkg-config", "cmake", "perl"].concat(gpui_libraries)),
				PackageEnv([("LD_LIBRARY_PATH", LibraryPath(gpui_runtime_libraries))]),
			],
		),
		Shell("default", [Use("dev")]),
		Shell("mobile", [Use("mobile")]),
		Shell("gpui", [Use("gpui")]),
		# The crates apps/buzz-gpui/Cargo.lock names, the buzz-app git crates
		# at the commit it records included, and the rest of that buzz-app
		# commit, for the files its plugin-manager crate reads from outside
		# itself.
		CargoVendor("gpui-crates", gpui_lock),
		Source("buzz-app", "github:benthecarman/buzz-app/219fea82f81de49d370cd4cd6930ad4a35eb49c0"),
		Build(
			"gpui",
			[
				Description("buzz-gpui, built offline from the vendored crates"),
				Use("gpui"),
				Inputs(["gpui-crates", "buzz-app"]),
				# The workspace and the root crates it depends on by path, as
				# the flake's source filter keeps them.
				Files([
					"Cargo.toml",
					"apps/buzz-gpui",
					"crates/buzz-core",
					"crates/buzz-sdk",
					"crates/buzz-session-control",
					"crates/buzz-remote-sessions",
				]),
				Run(["sh", "apps/buzz-gpui/scripts/kai-build.sh"]),
				Output("buzz-gpui-out"),
			],
		),
	].concat(pinned),
)
