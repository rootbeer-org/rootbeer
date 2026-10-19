# Distributing Rootbeer

The default `embedded-stdlib` feature packages the Lua standard library and type
metadata inside `rb`. The resulting binary can run without a separate Lua
installation or standard-library checkout.

```sh
cargo build --locked --release
```

Distributions that require Lua source files on disk can disable that feature and
choose a compile-time standard-library directory:

```sh
ROOTBEER_LUA_DIR=/usr/share/rootbeer/lua \
  cargo build --locked --release --no-default-features
```

Ship the repository's `lua/rootbeer/` tree under that directory. Users can override
the directory with `rb --lua-dir /path/to/lua apply`.

`rb` embeds the package index's initial TUF root, the index URL, and the
bootstrap channel's verification keys. Recipes are never bundled in the
executable. See [index hosting and trust](/contributing/package-hosting).

Ship `rb-helper` beside `rb`. The first command that needs `/opt/rb` runs
`sudo rb-helper setup <group>` from the directory `rb` lives in.

A build without `RB_SERIAL` never relaunches into a newer released `rb`.
