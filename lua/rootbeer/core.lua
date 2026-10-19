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

--- @class rootbeer.PackageOptions
--- @field asset? string Exact GitHub release asset filename. Required when platform selection is ambiguous.
--- @field bins? table<string, string> Binary name → relative path in the GitHub asset. Defaults to executable discovery for archives, or the repository name for raw binaries.

--- @class rootbeer.PackageRequestSpec
--- @field request string Package name, name@version, or github:owner/repo@tag.
--- @field asset? string Exact release asset filename for a github: request.
--- @field bins? table<string, string> Exported command name → relative path for a github: request.

--- Builds packages from a checkout of recipes, laid out like the package
--- index with `packages/<name>.lua` and `host.lua`. A name the checkout
--- defines comes from its recipe instead of the index, and everything else
--- still comes from the index. A build the index publishes under the same key
--- is downloaded, and the rest builds on this machine.
--- @param path string The checkout, relative to the config.
function rootbeer.package_catalog(path) end

--- Installs a command-line tool, such as `"ripgrep"`. Use `"name@version"`
--- to choose an exact version. Package versions are saved in `rootbeer.lock`.
--- You can also use `github:owner/repo@tag` for a GitHub release's asset.
--- @param spec string|rootbeer.PackageRequestSpec Package request, or a request with options.
--- @param opts? rootbeer.PackageOptions Asset options for `github:` requests.
function rootbeer.package(spec, opts) end

--- Declares a dense package list in order. Accepts the same entries as package().
--- Validates every entry before adding operations; errors identify the invalid index.
--- @param specs (string|rootbeer.PackageRequestSpec)[] Package declarations.
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
