return {
	name = "finder",
	aliases = { "fnd" },
	description = "Find entries",
	homepage = "https://example.com/finder",
	recipe_maintainers = { "tale" },
	default_license = "MIT OR Apache-2.0",
	upstream = {
		github = "example/finder",
		repository_id = 1234,
		tag = "v{version}",
	},
	prebuilt = {
		github = "example/finder",
		asset = "finder-{tag}-{target}.tar.gz",
	},
	outputs = {
		bins = { "finder" },
		checks = {
			{ "finder", "--version" },
		},
	},
	platforms = {
		["x86_64-linux"] = {
			target = "x86_64-unknown-linux-musl",
			default_version = "10.5.0",
		},
	},
	versions = {
		["10.5.0"] = {
			digests = {
				["x86_64-linux"] = "3333333333333333333333333333333333333333333333333333333333333333",
			},
			license = "MIT",
		},
	},
}
