return {
	name = "ziggy",
	description = "A Zig program",
	homepage = "https://example.com/ziggy",
	default_license = "MIT",
	source = {
		url = "https://example.com/ziggy/archive/{commit}.tar.gz",
		strip_prefix = "ziggy-{commit}",
	},
	build = {
		backend = "zig",
		args = { "-Doptimize=ReleaseSafe", "-Dversion={version}" },
	},
	outputs = {
		bins = { "ziggy" },
	},
	platforms = {
		["x86_64-linux"] = { default_version = "0.1.0-dev.1" },
	},
	versions = {
		["0.1.0-dev.1"] = {
			digests = {
				["x86_64-linux"] = "9999999999999999999999999999999999999999999999999999999999999999",
			},
			commit = "294212ebd35f5b755062186a66bcfd6436d3627a",
		},
	},
}
