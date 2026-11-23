return {
	name = "pages",
	description = "Number pages",
	homepage = "https://example.com/pages",
	default_license = "MIT",
	source = {
		url = "https://example.com/pages-{version}.tar.bz2",
		archive = "tar.bz2",
		strip_prefix = "pages-{version}",
		patches = {
			"--- a/Makefile\n+++ b/Makefile\n@@ -1 +1 @@\n-CFLAGS = -O3\n+CFLAGS = -O2\n",
		},
	},
	build = {
		backend = "custom",
		dependencies = {
			{ package = "rb-unpack", kind = "build" },
		},
		steps = {
			install = {
				{ "make", "install", "PREFIX={prefix}" },
			},
		},
	},
	outputs = {
		bins = { "pages" },
	},
	platforms = {
		["aarch64-macos"] = { default_version = "2.0" },
		["x86_64-linux"] = { default_version = "2.0" },
	},
	versions = {
		["2.0"] = {
			digests = {
				["aarch64-macos"] = "2222222222222222222222222222222222222222222222222222222222222222",
				["x86_64-linux"] = "2222222222222222222222222222222222222222222222222222222222222222",
			},
			trees = {
				["aarch64-macos"] = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
				["x86_64-linux"] = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
			},
		},
		["1.0"] = {
			digests = {
				["aarch64-macos"] = "1111111111111111111111111111111111111111111111111111111111111111",
				["x86_64-linux"] = "1111111111111111111111111111111111111111111111111111111111111111",
			},
		},
	},
}
