return {
	name = "lister",
	description = "List entries",
	homepage = "https://example.com/lister",
	default_license = "MIT",
	prebuilt = {
		url = "https://example.com/lister-{version}-{target}.zip",
		install = {
			Archive = { format = "Zip", strip_prefix = "lister-{version}" },
		},
		dependencies = {
			{ package = "7zip", version = "26.04", kind = "build" },
		},
		steps = {
			prepare = {
				{ "rm", "-r", "completions" },
			},
			install = {
				{ "chmod", "-R", "a-w", "doc" },
			},
		},
	},
	outputs = {
		bins = { lister = "bin/lister" },
	},
	platforms = {
		["x86_64-linux"] = {
			target = "linux-amd64",
			default_version = "3.2.0",
		},
	},
	versions = {
		["3.2.0"] = {
			digests = {
				["x86_64-linux"] = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
			},
		},
	},
}
