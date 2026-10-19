# Getting started

Install Rootbeer, then run your first tool. A Lua configuration is optional.

## Install Rootbeer

Rootbeer supports Apple silicon macOS, Linux ARM64, and Linux x86-64. The installer only
needs `curl` (or `wget`) and `tar`.

```sh
sh -c "$(curl -fsSL https://rbpkg.com/rb.sh)"
eval "$("/opt/rb/profiles/$(id -u)/user/bin/rb" env)"
```

The installer downloads a bootstrap `rb`, which installs the published `rootbeer` package
into your user profile; from then on `rb` updates itself with `rb self-update`. Add the
`eval` line to your shell profile to keep installed commands on `PATH`.

Packages live in a store shared by every user at `/opt/rb`. The first command
that needs it asks for `sudo` once per machine to set it up, printing the exact
command first; without a terminal it prints it and exits instead. Members of the
admin group (`admin` on macOS, `sudo` or `wheel` on Linux) can then install
without `sudo`. A later Rootbeer update asks again only if it needs a newer store
helper.

Intel macOS is unsupported.

## Run a package

```sh
rb run jq -- --version
```

Rootbeer downloads and runs `jq`, caching it for later use. Arguments after `--`
go to the tool. No `rb init` or shell configuration is needed.

[Browse packages](https://search.rbpkg.com) to find another tool and check its versions,
commands, and supported platforms.

## Keep tools installed

```sh
rb use jq ripgrep
eval "$(rb env)"
rg --version
```

`rb use` installs tools for your user. The `ripgrep` package provides the `rg`
command. Add these lines to `~/.bashrc` for Bash or `~/.zshrc` for Zsh to make
your tools available in new terminals:

```sh
eval "$("/opt/rb/profiles/$(id -u)/user/bin/rb" env)"
```

See [using packages](/guide/packages) for exact versions, additional commands,
and [Zsh integration](/guide/packages#set-up-your-shell).

## Create your configuration

When you want to manage dotfiles and settings too, create a Lua configuration:

```sh
rb init
rb edit
```

Edit `~/.config/rootbeer/init.lua`:

```lua
local rb = require("rootbeer")

rb.package("ripgrep")
rb.file("~/.inputrc", "set editing-mode vi\n")
```

This declares ripgrep and configures Readline to use Vi editing keys. Packages
declared here are managed separately from those installed with `rb use`.

## Apply your configuration

Preview the planned changes, then apply them:

```sh
rb apply --dry-run
rb apply
```

Continue with [your configuration](/guide/configuration) to split files, use
modules, and keep settings in Git.

## Use an existing configuration

`rb init` can also start from a Git repository or a local directory. See
[reuse a configuration](/guide/configuration#reuse-a-configuration) for examples.

## Update

Run `rb self-update` to update Rootbeer through the signed package index. If Rootbeer
is declared in your Lua configuration, use `rb apply --update` instead.
To update tools installed with `rb use`, name them, or omit names to update all:

```sh
rb use --update jq ripgrep
```

Use `rb apply --update` for packages declared in Lua. See
[updates and offline use](/guide/package-locks) for saved versions and locks.

If you installed Rootbeer when it used `/opt/rootbeer`, run `rb migrate` once to
move your packages into `/opt/rb`.

## Uninstall

Remove the `eval` line from your shell profile, then delete your Rootbeer state
and profiles:

```sh
rm -rf ~/.local/state/rootbeer ~/.local/share/rootbeer
sudo rm -rf "/opt/rb/profiles/$(id -u)"
```

Your configuration in `~/.config/rootbeer` and any files it applied stay in place;
delete them yourself if you no longer want them. On macOS, also remove app links
Rootbeer added to `~/Applications`. Once no other user on the machine uses Rootbeer,
remove the shared store:

```sh
sudo rm -rf /opt/rb
```
