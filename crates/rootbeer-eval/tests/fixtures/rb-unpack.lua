return {
	name = "rb-unpack",
	description = "Unpack archives",
	homepage = "https://rbpkg.com",
	default_license = "MIT",
	prebuilt = {
		url = "https://example.com/rb-unpack-{version}-{target}.tar.gz",
	},
	outputs = {
		bins = { "rb-unpack" },
	},
	platforms = {
		["aarch64-macos"] = {
			target = "macos",
			default_version = "0.1.0",
		},
		["x86_64-linux"] = {
			target = "linux",
			default_version = "0.1.0",
		},
	},
	versions = {
		["0.1.0"] = {
			digests = {
				["aarch64-macos"] = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
				["x86_64-linux"] = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
			},
		},
	},
}
