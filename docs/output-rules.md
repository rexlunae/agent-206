# Output rules

Output rules shorten tool results before a native run's model sees them, so
build logs and command output use less context. They are implemented in
`crates/output-rules` and applied by the run engine (see
[run-engine.md](run-engine.md#output-rules)). The rule model follows RustyClaw's
`tokenjuice` crate; Agent 206 rules are TOML.

## Where rules come from

1. **Built-in rules** ship with the application in
   `crates/output-rules/rules/builtin.toml`. They apply only to
   `execute_command`:

   | Id | Applies to | What it does |
   | --- | --- | --- |
   | `git.status` | `git status` | Drops `(use "git …")` hints and blank runs; keeps the branch and every path; counts modified and untracked paths |
   | `git.diff` | `git diff`, `git show` | Keeps the first 400 and last 100 lines of a longer diff |
   | `cargo` | `cargo build/check/test/clippy/run/nextest` | Drops progress lines and passing tests; keeps warnings, errors, failures and results; counts them |
   | `npm.install` | `npm`/`pnpm`/`yarn` `install`, `i`, `add`, `ci` | Drops spinners, progress and deprecation notices |
   | `ls` | `ls` | Keeps the first 300 and last 30 lines of a longer listing |

2. **User rules** are `*.toml` files directly in `output-rules/` in the
   gateway's state directory (next to `settings.toml`), read in file-name
   order. A user rule with a built-in rule's `id` replaces it, and
   `enabled = false` turns a rule off.

Files in a project cannot add or change rules: what the model is shown is the
user's choice, not the repository's.

Rules are read when a run starts, so an edited file applies from the next run.
A file that cannot be read, is larger than 256 KiB, is not valid TOML, has an
unknown key, a rule without an `id`, an id or counter name over 128 bytes, more than 16 counters, an invalid pattern, or an `id` used twice
across user files fails the run before the model is asked, with the file and
the cause in the error. At most 256 user rules are loaded.

## Format

```toml
[[rule]]
id = "make"                 # required; replaces a built-in rule with this id
family = "build"            # optional grouping
description = "Drop make's directory chatter."
priority = 60               # higher is tried first; default 0
enabled = true              # default true

[rule.match]                # every condition given must hold
tools = ["execute_command"] # any of these tool names
argv0 = ["make", "gmake"]   # any of these programs
git_subcommands = []        # `git` with any of these subcommands
argv_all = [["-j"]]         # every group's words appear among the arguments
argv_any = [["all"], ["install"]] # some group's words all appear
command_all = []            # the command line contains all of these
command_any = []            # the command line contains one of these

[rule.filters]
skip = ['^make\[\d+\]: (Entering|Leaving) directory']  # drop matching lines
keep = []                   # when given, keep only matching lines (after skip)

[rule.transforms]
strip_ansi = true           # remove terminal escape sequences
dedupe_adjacent = false     # drop a line identical to the one before it
fold_blank_runs = true      # collapse runs of blank lines into one
trim_empty_edges = true     # drop blank lines at the start and end

[rule.summarize]            # when more than head + tail lines remain
head = 200
tail = 50

[[rule.counters]]           # counted over the lines kept
name = "errors"
pattern = '^.*error'
flags = "i"                 # i, m, s or x
```

Patterns are Rust `regex` syntax, which runs in linear time. A rule with no
`[rule.match]` applies to every tool call.

The program a rule matches is found in the call's `command` argument: the last
command of a `;`, `&&` or `||` list, the first command of a pipeline, without
leading `NAME=value` assignments or `env`, `time`, `command`, `exec`, `nice` or
`nohup`, and without its directory. Quotes are honoured; nothing is expanded.
So `cd crate && RUST_LOG=debug cargo test 2>&1 | tail` matches as
`cargo test 2>&1`.

## What the model and the user see

The highest-priority matching rule applies; at equal priority a user rule
wins. When it changes the result, the reduced text ends with a line such as
``[output rule `cargo` kept 12 of 340 lines; warnings: 1; errors: 0]``. Elided
middles read `… [N lines left out]`. The reduced text replaces the result for
both the model and the conversation; the call records which rule applied and
the original size, and the tool trace shows "Shortened for the model by the
built-in output rule `cargo` from N bytes". Results only useful whole (skill
instructions, plans, extension listings) are never reduced.

`execute_command` cuts each stream to its limit before a rule sees the result,
so a rule shortens what was kept, not the command's whole output.

## Not implemented

RustyClaw's project layer (`<workspace>/.tokenjuice/rules/`) is deliberately
absent. There is no GUI or CLI to list or edit rules yet; edit the files.
