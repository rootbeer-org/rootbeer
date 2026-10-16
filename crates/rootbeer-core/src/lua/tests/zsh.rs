//! Tests for `lua/rootbeer/zsh.lua`.

use crate::lua::test_support::run;
use crate::plan::Op;

/// Helper: collect all WriteFile ops as `(path-suffix, content)` pairs.
/// Only inline (`WriteSource::Bytes`) UTF-8 sources are captured — secret-
/// backed sources don't carry content at plan time.
fn writes(ops: &[Op]) -> Vec<(String, String)> {
    ops.iter()
        .filter_map(|op| match op {
            Op::WriteFile { path, source } => source
                .as_str()
                .map(|c| (path.display().to_string(), c.to_string())),
            _ => None,
        })
        .collect()
}

fn find<'a>(writes: &'a [(String, String)], suffix: &str) -> &'a str {
    &writes
        .iter()
        .find(|(p, _)| p.ends_with(suffix))
        .unwrap_or_else(|| panic!("no write for {suffix}; got: {writes:?}"))
        .1
}

#[test]
fn zsh_config_writes_bootstrap_and_zshrc() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({
            keybind_mode = "emacs",
            options = { "CORRECT", "EXTENDED_GLOB" },
            env = { EDITOR = "nvim" },
            aliases = { g = "git" },
        })
        "#);

    let writes = writes(&ops);
    // Bootstrap, env, package profile, zprofile, zshrc — at minimum.
    let bootstrap = find(&writes, ".zshenv");
    assert!(bootstrap.contains("ZDOTDIR=~/.config/zsh"));
    assert!(bootstrap.contains(". $ZDOTDIR/.zshenv"));

    let zprofile = find(&writes, ".zprofile");
    assert!(zprofile.contains(". "));
    assert!(zprofile.contains("rootbeer/env.sh"));
    assert!(writes.iter().any(|(p, content)| {
        p.ends_with("rootbeer/env.sh") && content.contains("export PATH=")
    }));

    let zshrc = find(&writes, ".zshrc");
    assert!(zshrc.contains("set -o emacs"));
    assert!(zshrc.contains("setopt CORRECT"));
    assert!(zshrc.contains("setopt EXTENDED_GLOB"));
    assert!(zshrc.contains("alias g='git'"));
}

#[test]
fn zsh_config_without_environment_writes_the_bootstrap_target() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({})
    "#);
    let writes = writes(&ops);
    assert_eq!(find(&writes, "/.config/zsh/.zshenv"), "\n");
}

#[test]
fn zsh_config_respects_custom_dir() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({ dir = "/etc/zsh", env = { LANG = "C" } })
        "#);
    let writes = writes(&ops);
    let bootstrap = find(&writes, ".zshenv");
    assert!(bootstrap.contains("ZDOTDIR=/etc/zsh"));
    // env file lives under custom dir
    assert!(
        writes.iter().any(|(p, _)| p == "/etc/zsh/.zshenv"),
        "expected /etc/zsh/.zshenv, got {writes:?}"
    );
}

#[test]
fn zsh_history_block_includes_share_dedup_append_defaults() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({ history = {} })
        "#);
    let writes = writes(&ops);
    let zshrc = find(&writes, ".zshrc");
    assert!(zshrc.contains("setopt SHARE_HISTORY"));
    assert!(zshrc.contains("setopt HIST_IGNORE_DUPS"));
    assert!(zshrc.contains("HISTFILE=$ZDOTDIR/.zsh_history"));
    assert!(zshrc.contains("HISTSIZE=10000"));
}

#[test]
fn zsh_function_body_preserves_blank_lines() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({
            functions = {
                demo = [=[
local a=1

if [[ -n "$a" ]]; then
	echo "$a"
fi

echo "done"]=],
            },
        })
        "#);
    let writes = writes(&ops);
    let zshrc = find(&writes, ".zshrc");
    let expected = "\
demo() {
\tlocal a=1

\tif [[ -n \"$a\" ]]; then
\t\techo \"$a\"
\tfi

\techo \"done\"
}";
    assert!(
        zshrc.contains(expected),
        "expected blank lines and indentation preserved.\n--- expected ---\n{expected}\n--- got ---\n{zshrc}"
    );
}

#[test]
fn zsh_aliases_emit_in_sorted_order() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({
            aliases = { z = "zoxide", a = "apt", m = "make", g = "git" },
        })
        "#);
    let writes = writes(&ops);
    let zshrc = find(&writes, ".zshrc");
    let a = zshrc.find("alias a=").expect("alias a");
    let g = zshrc.find("alias g=").expect("alias g");
    let m = zshrc.find("alias m=").expect("alias m");
    let z = zshrc.find("alias z=").expect("alias z");
    assert!(a < g && g < m && m < z, "aliases not sorted: {zshrc}");
}

#[test]
fn zsh_functions_emit_in_sorted_order() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({
            functions = {
                zulu = "echo z",
                alpha = "echo a",
                mike = "echo m",
            },
        })
        "#);
    let writes = writes(&ops);
    let zshrc = find(&writes, ".zshrc");
    let a = zshrc.find("alpha()").expect("alpha");
    let m = zshrc.find("mike()").expect("mike");
    let z = zshrc.find("zulu()").expect("zulu");
    assert!(a < m && m < z, "functions not sorted: {zshrc}");
}

#[test]
fn zsh_evals_and_sources_are_emitted() {
    let ops = run(r#"
        local zsh = require("rootbeer.zsh")
        zsh.config({
            evals = { "mise activate zsh" },
            sources = { "~/.config/zsh/local.zsh" },
        })
        "#);
    let writes = writes(&ops);
    let zshrc = find(&writes, ".zshrc");
    assert!(zshrc.contains(r#"eval "$(mise activate zsh)""#));
    assert!(zshrc.contains("source ~/.config/zsh/local.zsh"));
}
