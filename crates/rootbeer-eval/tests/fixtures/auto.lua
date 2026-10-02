return {
	name = "auto",
	description = "An autotools program",
	homepage = "https://example.com/auto",
	default_license = "GPL-3.0-or-later",
	source = {
		url = "https://example.com/auto-{version}.tar.gz",
		strip_prefix = "auto-{version}",
	},
	build = {
		backend = "autotools",
		configure = { "--disable-nls", "--with-z={dependencies.libz}" },
		dependencies = {
			{ package = "libz", version = "1.3.2", kind = "link_runtime" },
		},
	},
	outputs = {
		bins = { "auto" },
	},
	platforms = {
		["x86_64-linux"] = { default_version = "5.8" },
	},
	versions = {
		["5.8"] = {
			digests = {
				["x86_64-linux"] = "1111111111111111111111111111111111111111111111111111111111111111",
			},
		},
	},
}
