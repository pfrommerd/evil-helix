<div align="center">

<h1>evil-helix</h1>

**pfrommerd's souped-up evil-helix** — a personal soft fork of
[evil-helix](https://github.com/usagi-flow/evil-helix) with a built-in file tree,
automatic file reloading, and opinionated keyboard-first defaults.

[![Build status](https://img.shields.io/github/actions/workflow/status/usagi-flow/evil-helix/evil-build-tag.yml?style=for-the-badge&logo=github)](https://github.com/usagi-flow/evil-helix/actions/workflows/evil-build-tag.yml)

![Screenshot](./screenshot.png)

<hr />

</div>

## Additional features and defaults

This fork includes several features and default bindings beyond evil-helix:

- **Shifted navigation cluster:** normal and visual navigation uses `j`, `k`, `l`, `;` for
  left, down, up, and right instead of `h`, `j`, `k`, `l`.
- **Persistent file tree:** `t` or `Space-t` shows and focuses the right-side tree; `T` or
  `Space-T` toggles its visibility. The active file is revealed and centered when the tree gains
  focus.
- **Fast fuzzy tree search:** `Space-q` opens the tree directly into its top-line search field.
  Within the tree, `q` or `Space-q` toggles search focus. Results preserve their directory context,
  flatten single-child directory chains, and use the same asynchronous file index as the regular
  `Space-f` picker.
- **Tree navigation and operations:** in the evil keymap, use `l`/`k` to move up/down,
  `j`/`;` to collapse/expand, `H`/`M`/`L` for the top/middle/bottom of the view, and `g`/`G`
  for the beginning/end of the tree. `Enter`, `Ctrl-s`, and `Ctrl-v` open files normally or in
  splits; `.` toggles hidden files, which are hidden by default.
- **Automatic file reload:** native filesystem watching is enabled by default. Clean buffers are
  automatically reloaded when their files change on disk; modified buffers are never overwritten.
- **Commands from pickers and the tree:** `:` temporarily opens the command line without closing
  the underlying picker or losing tree/search focus, so commands such as `:q` work there too.

The tree and editor bindings remain configurable through the normal Helix keymap and editor
settings.

## Installation

[Download a package](https://github.com/usagi-flow/evil-helix/releases) and extract it in `/opt`. Additionally, it's recommended to symlink it in `/usr/local/bin`:

```sh
cd /opt
sudo curl -Lo helix.tar.gz https://github.com/usagi-flow/evil-helix/releases/download/release-<VERSION>/helix-<ARCH>-<OS>.tar.gz
sudo tar -xf helix.tar.gz
cd /usr/local/bin
sudo ln -sv /opt/helix/hx .
```

### Package manager

If a package is available for your system's package manager, it's the recommended way to install evil-helix.

[![Packaging status](https://repology.org/badge/vertical-allrepos/evil-helix.svg)](https://repology.org/project/evil-helix/versions)

## Current state

These are the current differences compared to the upstream project:

-	Vim keybindings (_feel free to file an issue if you're missing certain bindings_):
	-	Commands: `a`, `c`, `d`, `y`, `x`
	-	Modifiers: `i`
	-	Motions: `w`, `W`, `e`, `E`, `b`, `B`, `0`, `$`
	-	Visual line mode: `V`
-	Adjusted defaults ([511060a](https://github.com/usagi-flow/evil-helix/commit/511060abcfcbe9377ec50e8a0ecaf4c0660776bb)):
	-	The Helix "SEL" mode is called "VIS"
	-	Smart tab is disabled by default
	-	Navigation uses `j`, `k`, `l`, `;` for left, down, up, and right
	-	Indent guides are enabled
-	Basic Vim modeline support ([#3](https://github.com/usagi-flow/evil-helix/pull/3))
-	Support for colored/rainbow indentation guides, _opt-in: see PR_ ([#76](https://github.com/usagi-flow/evil-helix/pull/76))
-	If `color_modes` is enabled, color the file type in the statusline as well ([5503542](https://github.com/usagi-flow/evil-helix/commit/5503542c0314936ea91464f2944666ed42fea86c))
-	Minimalistic window separator ([dd990ca](https://github.com/usagi-flow/evil-helix/commit/dd990cad1cb92a024321aca19728c68cb066dd09))

Moreover, evil-helix introduces the `editor.evil` option, which is `true` by default. It can be set to false to completely deactivate evil-helix behavior without having to use a different build:

```toml
[editor]
evil = true # Default; set this to `false` to disable evil-helix behavior
```

## Project philosophy

### Configurable features instead of plugins

This fork seeks to implement functionality as part of the editor, and make it configurable.
The added functionality includes a Vim look-and-feel, but also other features.

In contrast, the upstream project, Helix, mostly limits its scope to its current core functionality, and defers further functionality to the future Scheme-based plugin system.

Compared to plugins, implementing features as part of the editor greatly improves performance, and avoids the risk of plugin compatibility issues.

### Sensible defaults

In addition, sensible defaults are crucial:
The editor must offer a wide range of tools for your job, but it must do what you expect an editor to do.

### Avoid Scheme/Lisp

Scheme/Lisp should not be forced onto the user.
It's error-prone and harder to read by humans, compared to Rust/TOML/Lua/...

If upstream Helix moves to a [Scheme-based configuration](https://github.com/helix-editor/helix/issues/10389),
this project will seek to keep a user-friendly alternative.

### Soft fork

This project is a "soft fork", i.e. it remains compatible with the upstream and regularly rebases its changes on top of the upstream master branch. New features should be carefully isolated from the upstream codebase in order to avoid conflicts.

Whether this project remains in this state will depend on how much this project's philosophy and the upstream project diverge, although a hard fork should be considered as a last resort.

### Small and regular version releases

Considering the kind and frequency of changes to this repository, it makes sense to release small changes often, rather than holding features back in large releases. Releases are currently tagged on-demand.

## Project goals

-	Move the project into an organization and prepare a website
-	Introduce blackbox tests (cf. [#68](https://github.com/usagi-flow/evil-helix/issues/68))
-	Introduce more Vim keybindings
-	Implement more common/crucial features as part of the editor:
	-	Light/dark mode support
-	Maintain compatibility with upstream
	-	Contribute features to upstream where possible
	-	Ensure (through CI) that rebasing is always possible

## Development

Keep in mind the `main` branch may be rebased onto the upstream `master` branch.
