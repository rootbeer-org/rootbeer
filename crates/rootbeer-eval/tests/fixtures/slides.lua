return {
	name = "slides",
	description = "Show slides",
	homepage = "https://example.com/slides",
	default_license = "Proprietary",
	prebuilt = {
		url = "https://example.com/Slides-{version}.zip",
		dependencies = {
			{ package = "rb-unpack", kind = "build" },
		},
	},
	outputs = {
		apps = { ["Slides.app"] = "Slides.app" },
		bins = { slides = "Slides.app/Contents/MacOS/slides" },
	},
	platforms = {
		["aarch64-macos"] = { default_version = "2.0" },
		["x86_64-linux"] = {
			default_version = "2.0",
			prebuilt = {
				url = "https://example.com/slides-{version}.tar.gz",
				install = {
					Archive = {
						format = "TarGz",
						strip_prefix = "slides-{version}",
					},
				},
				dependencies = {
					{ package = "rb-unpack", kind = "build" },
				},
			},
			outputs = {
				bins = { slides = "bin/slides" },
			},
		},
	},
	versions = {
		["2.0"] = {
			digests = {
				["aarch64-macos"] = "2222222222222222222222222222222222222222222222222222222222222222",
				["x86_64-linux"] = "4444444444444444444444444444444444444444444444444444444444444444",
			},
			trees = {
				["aarch64-macos"] = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
				["x86_64-linux"] = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
			},
		},
		["1.0"] = {
			digests = {
				["aarch64-macos"] = "1111111111111111111111111111111111111111111111111111111111111111",
			},
		},
	},
}
