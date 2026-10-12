return {
	name = "rust",
	description = "Build Rust",
	homepage = "https://www.rust-lang.org",
	default_license = "MIT OR Apache-2.0",
	prebuilt = { url = "https://example.com/cargo-{version}" },
	outputs = {
		bins = { "cargo" },
	},
	platforms = {
		["aarch64-macos"] = { default_version = "1.98.1" },
	},
	versions = {
		["1.98.1"] = {
			digests = {
				["aarch64-macos"] = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
			},
		},
	},
}
