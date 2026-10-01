return {
	name = "tool",
	description = "A raw binary",
	homepage = "https://example.com/tool",
	default_license = "MIT",
	prebuilt = {
		url = "https://example.com/tool-{version}-{target}",
		mirror = true,
	},
	outputs = {
		bins = { "tool" },
	},
	platforms = {
		["aarch64-macos"] = {
			target = "darwin-arm64",
			default_version = "2.0.0",
		},
		["x86_64-linux"] = { target = "linux-amd64", default_version = "2.0.0" },
	},
	versions = {
		["2.0.0"] = {
			digests = {
				["aarch64-macos"] = "7777777777777777777777777777777777777777777777777777777777777777",
				["x86_64-linux"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
			},
		},
	},
}
