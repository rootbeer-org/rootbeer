return {
	name = "crab",
	description = "A Rust program",
	homepage = "https://example.com/crab",
	default_license = "MIT",
	source = {
		url = "https://example.com/crab-{version}.zip",
		archive = "zip",
	},
	build = {
		backend = "rust",
		rust = {
			packages = { "crab-cli" },
			features = { "fast", "pretty" },
			no_default_features = true,
			environment = { CRAB_VERSION = "{version}" },
		},
	},
	outputs = {
		bins = { "crab", "crabd" },
	},
	platforms = {
		["aarch64-macos"] = { default_version = "1.0.0" },
	},
	versions = {
		["1.0.0"] = {
			digests = {
				["aarch64-macos"] = "2222222222222222222222222222222222222222222222222222222222222222",
			},
		},
	},
}
