return {
	name = "libz",
	description = "Compress things",
	homepage = "https://example.com/libz",
	recipe_maintainers = { "tale" },
	default_license = "Zlib",
	source = {
		url = "https://example.com/libz-{version}.tar.xz",
		archive = "tar.xz",
		strip_prefix = "libz-{version}",
		patches = {
			"--- a/Makefile\n+++ b/Makefile\n@@ -1 +1 @@\n-CFLAGS = -O3\n+CFLAGS = -O2\n",
		},
	},
	build = {
		backend = "custom",
		dependencies = {
			{ package = "tool", version = "2.0.0", kind = "all" },
		},
		libraries = { "lib/libz.a", "lib/libz.{shared_extension}" },
		steps = {
			configure = {
				{ "./configure", "--prefix=/", "--with-tool={dependencies}" },
			},
			build = {
				{ "make", "-j{jobs}", "VERSION={major}.{minor}" },
			},
			install = {
				{ "make", "DESTDIR={prefix}", "install" },
				{
					"sh",
					"-c",
					'echo "$1" > {prefix}/note',
					"note",
					"it's `done`",
				},
			},
		},
	},
	outputs = {
		checks = {
			{ "find", ".", "-exec", "true", "{}", ";" },
		},
	},
	platforms = {
		["aarch64-macos"] = { default_version = "1.3.2" },
		["x86_64-linux"] = { default_version = "1.3.2" },
	},
	versions = {
		["1.3.2"] = {
			digests = {
				["aarch64-macos"] = "6666666666666666666666666666666666666666666666666666666666666666",
				["x86_64-linux"] = "6666666666666666666666666666666666666666666666666666666666666666",
			},
			revision = 3,
		},
	},
}
