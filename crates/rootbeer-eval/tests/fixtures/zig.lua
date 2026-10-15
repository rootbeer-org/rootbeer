return {
	name = "zig",
	description = "Build Zig",
	homepage = "https://ziglang.org",
	default_license = "MIT",
	prebuilt = {
		url = "https://example.com/zig-{target}-{version}.tar.xz",
		install = {
			Archive = {
				format = "TarXz",
				strip_prefix = "zig-{target}-{version}",
			},
		},
	},
	outputs = {
		bins = { zig = "zig" },
	},
	platforms = {
		["x86_64-linux"] = {
			target = "x86_64-linux",
			default_version = "0.16.0",
		},
	},
	versions = {
		["0.16.0"] = {
			digests = {
				["x86_64-linux"] = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
			},
		},
	},
}
