return {
	name = "go",
	description = "Build Go",
	homepage = "https://go.dev",
	default_license = "BSD-3-Clause",
	prebuilt = {
		url = "https://example.com/go{version}.{target}.tar.gz",
		install = {
			Archive = { format = "TarGz", strip_prefix = "go" },
		},
	},
	outputs = {
		bins = { go = "bin/go" },
	},
	platforms = {
		["x86_64-linux"] = {
			target = "linux-amd64",
			default_version = "1.27.1",
		},
	},
	versions = {
		["1.27.1"] = {
			digests = {
				["x86_64-linux"] = "4444444444444444444444444444444444444444444444444444444444444444",
			},
		},
	},
}
