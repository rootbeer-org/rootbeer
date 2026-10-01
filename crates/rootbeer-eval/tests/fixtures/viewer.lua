return {
	name = "viewer",
	description = "View things",
	homepage = "https://example.com/viewer",
	default_license = "Proprietary",
	prebuilt = {
		url = "https://example.com/Viewer-{version}.dmg",
	},
	outputs = {
		apps = { ["Viewer.app"] = "Viewer.app" },
		bins = { viewer = "Viewer.app/Contents/MacOS/viewer" },
	},
	platforms = {
		["aarch64-macos"] = { default_version = "4.1" },
	},
	versions = {
		["4.1"] = {
			digests = {
				["aarch64-macos"] = "8888888888888888888888888888888888888888888888888888888888888888",
			},
		},
	},
}
