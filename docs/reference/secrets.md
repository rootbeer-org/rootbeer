# Secrets

Read secrets from 1Password or age-encrypted files into generated configuration,
or decrypt whole files during apply.

```lua
local rb = require("rootbeer")
```

## Providers

### 1Password (`op`)

Rootbeer installs its own [1Password CLI](https://developer.1password.com/docs/cli),
`op@2.39.0` from the signed package index, into `/opt/rb` on the first provider
call. It runs that store executable directly, without using your shell's `op` or
changing any profile, and holds it from `rb gc` while `rb` runs. Field reads,
document downloads, and `identity_op` share it throughout planning and apply.

The CLI stays in the store until `rb gc`; secret values are never cached.
`--offline` can't install it, so providers that need it fail offline. First use
can download the CLI, including during a dry run that evaluates
`rb.secret.op()`. Deferred document and age-file operations do not prepare their
tools during a dry run.

Your normal 1Password account authentication still applies. Touch ID / biometric
prompts surface synchronously when the CLI runs. The provider version is pinned
by Rootbeer; `apply --update` does not change this internal dependency.

**Use a secret in a config file:**

```lua
local lines = {
    "[settings]",
    "debug = false",
    'api_url = "' .. rb.secret.op("op://Development/WakaTime/url") .. '"',
    'api_key = "' .. rb.secret.op("op://Development/WakaTime/credential") .. '"',
}
rb.file("~/.wakatime.cfg", table.concat(lines, "\n"))
```

**Save a document**, such as an SSH key or certificate:

```lua
rb.secret.op_document("op://Private/work-ssh-key", "~/.ssh/work_rsa", {
    mode = 0x180, -- 0o600
})
```

The `mode` option sets the file permissions so only your user can read and
write the key.

**Reference shapes.** Both functions accept `op://`-style references:

- `op://<vault>/<item>/<field>` — a specific field within an item.
- `op://<vault>/<item>` — used with `op_document` to fetch the document
  attached to an item.

### Age

Age decryption is built in; `rage` is only needed to encrypt source files.
Native X25519 identities (`AGE-SECRET-KEY-...`) are supported, including identity
files with comments and multiple keys. SSH keys, plugins, and passphrase-encrypted
identities or ciphertext are not supported. Binary and ASCII-armored ciphertext
are accepted.

Encrypt using the public recipient (safe to commit):

```sh
rage -r age1… -o private/sessionizer.zsh.age sessionizer.zsh
```

Commit the encrypted file, keeping plaintext and private keys outside Git.
For encrypted snippets, the decrypted text preserves all whitespace:

```lua
local identity = { identity = "~/.config/sops/age/keys.txt" }
local sessionizer = rb.secret.age("private/sessionizer.zsh.age", identity)
```

For whole files, decryption happens only during apply. Writes are atomic and
permissions default to `0600`; existing destination symlinks are replaced.

```lua
rb.secret.age_file("private/config.age", "~/.config/example/config", {
    identity = "~/.config/sops/age/keys.txt",
    mode = 0x180,
})
```

Instead of `identity`, use `identity_op = "op://Development/AGE Key/credential"`
to fetch a complete native private key from 1Password. This supports first-run
bootstrap without first writing a key file. Exactly one identity option is
required. Fields containing only the key suffix must be updated to contain the
full `AGE-SECRET-KEY-...` value. Identity file paths are resolved relative to the
script directory, with `~` expansion.

Snippet decryption needs its key during planning: a key file scheduled with
`rb.file()` in the same run does not yet exist. Whole-file decryption can use a
key written by an earlier operation during apply. Installed plaintext remains
readable locally according to its file permissions.

## Previewing changes

`rb.secret.op()` reads a value while evaluating your configuration, including
when you run `rb apply --dry-run`. You must be signed in to 1Password, and the
value becomes part of the in-memory generated file content. The CLI preview
shows paths and sizes, not file contents. `rb.secret.age()` has the same planning
behavior and needs access to its identity even in dry runs. File bytes are omitted
from plan debug output, but remain accessible to custom execution handlers.
Plain Lua strings are not automatically redacted if printed, included in errors,
or used in command arguments.

`rb.secret.op_document()` downloads the document only when applying changes.
A preview shows the destination without fetching or displaying its contents.
`rb.secret.age_file()` likewise defers decryption and key access until apply.

## API Reference

<!--@include: ../api/_generated/secret.md-->
