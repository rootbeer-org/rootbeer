--- @meta

--- @class rootbeer
--- Primitives for managing files, creating symlinks, and querying system information.

--- Information about the current host machine and user.
--- See `host.lua` for the full type definition.
--- @type rootbeer.HostInfo
rootbeer.host = {}

--- The first-class profile system. See `profile.lua` for the full type
--- definition.
--- @type profile
rootbeer.profile = {}

--- Absolute path to the rootbeer source directory (the directory containing
--- the entry-point script). Useful for commands that need to operate on the
--- source repo itself (e.g. `git remote set-url`).
--- @type string
rootbeer.source_dir = ""

--- Reads a UTF-8 file during planning. Relative paths resolve from the source
--- directory; paths starting with `~` expand to the home directory.
--- @param path string File to read.
--- @return string content File contents, including whitespace.
function rootbeer.read_file(path) end

--- Writes content to a file. Parent directories are created automatically.
--- Paths starting with `~` are expanded to `$HOME`; relative paths resolve
--- from the script directory.
--- @param path string The destination file path.
--- @param content string The content to write.
function rootbeer.file(path, content) end

--- Reads a file from disk at plan time and returns its contents as a string.
--- Useful for inlining shell snippets, templates, or any other text resource
--- alongside your Lua configuration (the Nix `builtins.readFile` equivalent).
--- Paths starting with `~` are expanded to `$HOME`; relative paths resolve
--- from the script directory. Reads happen immediately during plan, not
--- during apply.
--- @param path string The source file path.
--- @return string content The file's contents.
function rootbeer.read_file(path) end

--- Creates a symbolic link from a file in the script directory.
--- The source path is relative to the script directory and must exist.
--- The destination supports `~` expansion.
--- Idempotent — existing correct links are skipped, stale links are replaced.
--- @param src string Source path relative to the script directory.
--- @param dst string Destination path (supports `~` expansion).
function rootbeer.link_file(src, dst) end

--- Copies a file from the script directory to the destination, but only if
--- the destination does not already exist. Useful for seeding configuration
--- files that the user (or an application) is expected to modify after the
--- initial bootstrap (e.g. an editor config that records UI state).
--- The source path is relative to the script directory and must exist.
--- The destination supports `~` expansion. If the destination already exists,
--- the operation is reported as `skip`.
--- @param src string Source path relative to the script directory.
--- @param dst string Destination path (supports `~` expansion).
function rootbeer.copy_file(src, dst) end

--- Creates a symbolic link between arbitrary paths.
--- Both paths support `~` expansion and relative path resolution.
--- Unlike `link_file`, the source is not restricted to the script directory.
--- The source must exist at plan time.
--- @param src string Source path (supports `~` expansion).
--- @param dst string Destination path (supports `~` expansion).
function rootbeer.link(src, dst) end

--- Executes a command in the source directory. The command is deferred until the apply stage.
--- @param cmd string The command to run (e.g. `"brew"`).
--- @param args? string[] Optional arguments passed to the command.
function rootbeer.exec(cmd, args) end

--- @class rootbeer.PackageSpec
--- @field name string Package name.
--- @field version string Locked package version.
--- @field source rootbeer.PackageSource Locked package source.
--- @field install rootbeer.PackageInstall Package install recipe.
--- @field bins table<string, string> Binary name → relative path in the installed output tree.
--- @field apps? table<string, string> macOS application filename → relative .app bundle path. Apply manages links in ~/Applications.
--- @field output_sha256? string Verified installed-tree hash.
--- @field runtime_dependencies? table<string, rootbeer.PackageSpec> Exact runtime packages keyed by name@version; each requires output_sha256.

--- @class rootbeer.PackageSource
--- @field path? string Local directory tree source. The `sha256` is a deterministic tree hash.
--- @field file? string Local source file, usually an archive. The `sha256` is a byte hash.
--- @field url? string HTTPS, digest-pinned `ghcr://`, or `file://` source URL. The `sha256` is a byte hash.
--- @field sha256 string Locked source hash.

--- @class rootbeer.PackageInstall
--- @field dmg? boolean Copy declared application bundles from a read-only macOS disk image.
--- @field directory? boolean Install a directory tree source.
--- @field archive? "tar.gz"|"tgz"|"tar.xz"|"txz"|"zip" Install an archive source.
--- @field binary? string Install a raw executable at this relative path.
--- @field strip_prefix? string Relative subdirectory to use as the install root.

--- @class rootbeer.PackageOptions
--- @field source? boolean Build the selected approved release from source instead of using a prebuilt.
--- @field head? boolean Build the declared development branch, resolved to a commit in the lock.
--- @field tag? string Build this literal Git tag from the recipe's source repository.
--- @field rev? string Build this full lowercase Git commit SHA from the recipe's source repository.
--- @field branch? string Build this Git branch, resolved to a commit in the lock.
--- @field asset? string Exact GitHub release asset filename. Required when platform selection is ambiguous.
--- @field bins? table<string, string> Binary name → relative path in the GitHub asset. Defaults to executable discovery for archives, or the repository name for raw binaries.

--- @class rootbeer.PackageRequestSpec
--- @field source? boolean Build the selected approved release from source instead of using a prebuilt.
--- @field head? boolean Build the declared development branch, resolved to a commit in the lock.
--- @field tag? string Build this literal Git tag from the recipe's source repository.
--- @field rev? string Build this full lowercase Git commit SHA from the recipe's source repository.
--- @field branch? string Build this Git branch, resolved to a commit in the lock.
--- @field request string Package name, name@version, or github:owner/repo@tag.
--- @field asset? string Exact release asset filename for an explicit github: request.
--- @field bins? table<string, string> Exported command name → relative path for an explicit github: request.

--- Builds packages from a checkout of recipes, laid out like the package
--- index with `packages/<name>.lua` and `host.lua`. A name the checkout
--- defines comes from its recipe instead of the index, and everything else
--- still comes from the index. A build the index publishes under the same key
--- is downloaded, and the rest builds on this machine.
--- @param path string The checkout, relative to the config.
function rootbeer.package_catalog(path) end

--- Installs a command-line tool, such as `"ripgrep"`. Use `"name@version"`
--- to choose an exact version. Package versions are saved in `rootbeer.lock`.
--- You can also use `github:owner/repo@tag`, or a table describing an exact
--- download and installation.
--- @param spec string|rootbeer.PackageRequestSpec|rootbeer.PackageSpec Package name, source request with options, or exact package specification.
--- @param opts? rootbeer.PackageOptions Source selection for canonical packages, or asset options for explicit `github:` requests.
function rootbeer.package(spec, opts) end

--- Declares a dense package list in order. Accepts the same entries as package().
--- Validates every entry before adding operations; errors identify the invalid index.
--- @param specs (string|rootbeer.PackageRequestSpec|rootbeer.PackageSpec)[] Package declarations.
function rootbeer.packages(specs) end

--- Returns the configuration profile's stable binary directory without checking installation.
--- @return string path Configuration profile binary directory.
function rootbeer.bin_dir() end

--- Returns a command's stable path in the configuration profile without checking installation.
--- Declare the package before using this path in a deferred exec() call.
--- @param bin string Exported command name.
--- @return string path Configuration profile command path.
function rootbeer.bin_path(bin) end

--- Returns the stable Rootbeer profile path for a managed binary, or `nil`
--- when the binary is not provided by the current plan/profile. This never
--- searches the host `PATH`.
--- @param bin string Binary name.
--- @return string?
function rootbeer.which(bin) end

--- Writes a shell environment file and returns its path.
--- Source this file from your shell configuration to use installed commands.
--- @param shell? "sh"|"bash"|"zsh" Shell syntax to generate. Defaults to `"sh"`.
--- @return string
function rootbeer.env_export(shell) end

--- Checks whether a path exists (file, directory, or symlink).
--- Supports `~` expansion and relative paths.
--- @param path string The path to check.
--- @return boolean
function rootbeer.path_exists(path) end

--- Checks whether a path is a regular file.
--- Supports `~` expansion and relative paths.
--- @param path string The path to check.
--- @return boolean
function rootbeer.is_file(path) end

--- Checks whether a path is a directory.
--- Supports `~` expansion and relative paths.
--- @param path string The path to check.
--- @return boolean
function rootbeer.is_dir(path) end

--- Sets the `origin` remote URL for the rootbeer source directory.
--- The change is deferred until the apply stage. Idempotent — skipped when
--- the current URL already matches.
--- @param url string The desired remote URL (any git URL).
function rootbeer.remote(url) end

--- Pluggable secret providers (1Password and more).
--- See `secret.lua` for the full type definition.
--- @type rootbeer.secret
rootbeer.secret = {}

--- JSON codec. See `json.lua` for the full type definition.
--- @type rootbeer.json
rootbeer.json = {}

--- TOML codec. See `toml.lua` for the full type definition.
--- @type rootbeer.toml
rootbeer.toml = {}

--- YAML codec. See `yaml.lua` for the full type definition.
--- @type rootbeer.yaml
rootbeer.yaml = {}

--- Apple plist codec. See `plist.lua` for the full type definition.
--- @type rootbeer.plist
rootbeer.plist = {}

--- Script writers — executable scripts with shebang + chmod 0755.
--- See `scripts.lua` for the full type definition.
--- @type rootbeer.scripts
rootbeer.scripts = {}
