return {
	name = "7zip",
	description = "Unpack archives",
	homepage = "https://www.7-zip.org",
	default_license = "LGPL-2.1-or-later",
	prebuilt = {
		github = "ip7z/7zip",
		tag = "{version}",
		asset = "7z{major}{minor}-{target}.tar.xz",
	},
	outputs = {
		bins = { "7zz" },
	},
	platforms = {
		["aarch64-macos"] = {
			target = "mac",
			default_version = "26.04",
		},
		["x86_64-linux"] = {
			target = "linux-x64",
			default_version = "26.04",
			outputs = {
				bins = { ["7zz"] = "7zzs" },
			},
		},
	},
	versions = {
		["26.04"] = {
			digests = {
				["aarch64-macos"] = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
				["x86_64-linux"] = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
			},
		},
	},
}
