return {
	name = "gopher",
	description = "A Go program",
	homepage = "https://example.com/gopher",
	default_license = "Apache-2.0",
	source = {
		url = "https://example.com/gopher/archive/v{version}.tar.gz",
		strip_prefix = "gopher-{version}",
	},
	build = {
		backend = "go",
		dependencies = {
			{ package = "go", version = "1.27.1", kind = "build" },
		},
		go = {
			binaries = { gopher = "./cmd/gopher" },
			generate = { "./internal/gen" },
			experiments = { "greenteagc" },
			tags = { "netgo", "osusergo" },
			variables = { ["main.version"] = "{version}" },
		},
	},
	outputs = {
		bins = { "gopher" },
	},
	platforms = {
		["x86_64-linux"] = { default_version = "0.4.0" },
	},
	versions = {
		["0.4.0"] = {
			digests = {
				["x86_64-linux"] = "5555555555555555555555555555555555555555555555555555555555555555",
			},
		},
	},
}
