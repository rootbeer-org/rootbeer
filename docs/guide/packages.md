# Packages

[Browse packages](https://search.rbpkg.com) to find a tool, check its commands and supported
platforms, and choose a version. You can run it immediately, keep it installed,
or declare it in your dotfile configuration.

## Run a tool

No configuration or shell setup is required:

```sh
rb run jq -- --version
rb run ripgrep -- --hidden TODO .
```

Arguments after `--` go to the tool. Its commands are available on PATH for that
process and its children; your current shell stays unchanged. Its packages stay in
the store for later runs until `rb gc` removes them.

### Package names and commands

A package name identifies what to install. Its commands are the executables you
run: the `ripgrep` package provides `rg`.

```sh
rb run ripgrep -- --version
rb run xz --bin xzdec -- --version
```

Rootbeer runs the command named after the package, or the only command if the
package exports just one. Use `--bin` to select another exported command.

Add `-p` for each additional package the command needs. For example, benchmark
`jq` with `hyperfine`:

```sh
rb run hyperfine -p jq -- 'jq --version'
```

### macOS app bundles

Open an app preserved inside a package by naming its bundle:

```sh
rb run bobrwm --app Bobrwm.app
rb run bobrwm --app Bobrwm.app -- --config "$HOME/.config/bobrwm/config.zon"
```

`--app` opens that exact bundle from the verified package store without creating
an Applications link. Arguments after `--` go to the app;
macOS may reuse an already running instance. Use `--bin` separately for command-line
tools. App permissions, such as Bobrwm's Accessibility access, remain managed by
macOS.

To keep a declared app export available in `~/Applications`, install its package:

```sh
rb use bobrwm
```

Or declare `rb.package("bobrwm")` in Lua and run `rb apply`. Both install a copy
of the bundle at `~/Applications/Bobrwm.app`. On APFS the copy shares the store's
blocks, so it costs no space. Rootbeer replaces the copy whenever the profile
selects a different version, and `rb rollback` puts the old one back. An app
that's running keeps its old version until you restart it.

Some packages mark an app as updating itself, because its built-in updater
works. Rootbeer installs it once and then leaves updates to the app. It moves the
app to a newer packaged version only while the app hasn't updated itself, and
never back to an older one, even on rollback. `rb generations` shows the version
that's actually installed.

If an app Rootbeer updates changes on its own, Rootbeer warns and leaves it as it
is. Delete it to get Rootbeer's copy back, or run `rb apply --force-apps` to
replace every such app your configuration installs.

Existing apps are never overwritten, and an app Rootbeer didn't install makes
installation fail. Only one profile can install an app, so declaring it in Lua
and with `rb use` conflicts. Rootbeer does not configure login items or grant app
permissions.

Remove a package from your user profile with `rb unuse bobrwm`, which deletes
its app while its files stay in the store. `rb unuse bobrwm --keep-app` leaves
the app in place for you to manage. For Lua-managed apps, remove the declaration
and run `rb apply`.

## Keep tools installed

```sh
rb use jq ripgrep
eval "$(rb env)"
rg --version
```

`rb use` adds packages to your user profile. Requesting another version replaces
that package while keeping your other tools. The commands stay available in later
terminals once you add [shell setup](#set-up-your-shell).

This profile is independent of `init.lua` and `rb apply`. Its commands take
precedence when both profiles provide the same name. `rb rollback` switches it
back to its previous generation, and `rb generations` lists them.

`rb gc` deletes store outputs that no generation of any profile on the machine
still holds, and prints each one it removes. `rb gc --older-than 30d` first
deletes your generations older than 30 days, except each profile's current one.

## Declare tools in your configuration

Use Lua when you want to keep your package list alongside your shell and dotfiles:

```lua
local rb = require("rootbeer")

rb.packages({ "jq", "ripgrep", "gh" })
```

```sh
rb apply
eval "$(rb env)"
rg --version
```

Commit `init.lua` and the resulting `rootbeer.lock` to keep the configuration and
resolved package versions together.

Entries are request strings or request tables with GitHub options. The list
must be dense: no missing indexes or named keys.
Rootbeer validates the whole list before adding operations and identifies a bad
entry by its index. `rb.package(...)` accepts one entry when you only need one.

Keep platform and profile selection in normal Lua, using `rb.host` or
[`rb.profile.when`](/guide/profiles). See [other sources](/guide/package-sources)
for request tables and the [API reference](/reference/core#rootbeer-packages).

### Run a declared command during apply

Use its stable configuration-profile path even on the first installation:

```lua
rb.packages({ "gh" })
rb.exec(rb.bin_path("gh"), { "--version" })
```

`rb.bin_path()` constructs the path without checking whether it is installed yet;
`rb.exec()` runs during apply, after the earlier package declaration.
`rb.bin_dir()` returns the configuration profile's binary directory.
`rb.which()` is an optional query for a known or installed managed command and
returns `nil` when unavailable. It never searches the host PATH.

## Choose a version

An unversioned request selects the index's default for your platform when first
resolved. This is usually the newest packaged release; platforms an upstream has
dropped can have an older default. The package browser shows retained versions
and their platform support, so you can choose an exact release:

```sh
rb run jq@1.8.2 -- --version
rb use jq@1.8.2
```

The same syntax works in Lua: `rb.package("jq@1.8.2")`. Explicit versions never
fall back to another release. A retained version is available for selection;
it does not necessarily become the default.

A version can also name a release line by its leading parts. `node@24` selects
the newest 24.x release and `python@3.15` the newest 3.15.x:

```sh
rb use node@24
```

For more control, ask for a range. `^1.2` allows anything from 1.2 up to the
next major, `~1.2.3` anything up to the next minor, and `>=`, `>`, `<=`, `<`, and
`=` compare directly. Join bounds with a comma, and quote the request so your
shell leaves it alone:

```sh
rb use 'python@>=3.14,<3.16'
```

A line or range stays on what it first resolved to until you pass `--update`,
which moves it to the newest release it allows and never outside it.

`rb run` uses the current default each time. `rb use` and `rb apply` keep what
they installed until you pass `--update`, and exact versions stay fixed. See
[updates and offline use](/guide/package-locks) for each workflow.

## Set up your shell

Add these lines to `~/.bashrc` for Bash or `~/.zshrc` for Zsh:

```sh
eval "$("/opt/rb/profiles/$(id -u)/user/bin/rb" env)"
```

This runs `rb` from your user profile, where the installer puts it, and adds
commands from your user and configuration profiles. Fish syntax is not supported.

If you manage Zsh with Rootbeer, [`zsh.config()`](/modules/zsh) sets up package
commands automatically in login shells. Run `rb apply`, then `zsh -l` to load them.

## Platform support

Rootbeer installs prebuilt command-line tools on Apple silicon macOS and on Linux
ARM64 and x86-64. Intel macOS is unsupported. Filter the package browser for your
platform; support varies by version
and does not imply compatibility with every OS release or Linux distribution.
Installing from the index never compiles locally.

For tools outside the index, see [other package sources](/guide/package-sources).
Index packages may also export macOS app bundles. For other desktop apps and
service integration, use [Homebrew](/modules/brew) or another system package manager.
