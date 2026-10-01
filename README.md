# Herdr Git

![Herdr Git](assets/app.png)

A Git client for Herdr.

## Install

Requires Herdr 0.8.0 or later on macOS or Linux.

```sh
herdr plugin install JeHwanYoo/herdr-git
herdr plugin action invoke io.github.jehwanyoo.herdr-git.open
```

The second command toggles the Git pane.

To bind it to `prefix+u`, add this to `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "io.github.jehwanyoo.herdr-git.open"
description = "toggle Git pane"
```

Press `Ctrl+b`, then `u` to open the Git sidebar on the right.

## Features

| Feature | What you can do |
| --- | --- |
| Changes | Review changed files, stage or unstage them, and discard tracked changes |
| Diffs | View changes side by side and compare commits or branches |
| Graph | Browse and filter commit history with a lane graph |
| Files | Browse repository files, search file names and contents, and preview files with syntax highlighting, working-tree changes, blame, and line history |
| Blame & line history | See who changed selected lines and review their history |
| Copy | Copy selected code with its file path and line numbers |
| Commit | Create or amend a commit, or ask an Agent to write it |
| Remotes | Add, view, or remove remotes; fetch, pull, and choose where to push |
| Branches & tags | Create branches and tags, or switch branches |
| Rebase | Rebase onto a branch or commit, including interactive rebase |
| Cherry-pick, revert & reset | Apply a commit, undo it, or reset to it |
| Stash | Save uncommitted changes and restore them |
| Workspaces | Switch between repositories, Projects, and worktrees |

## Graph rendering

The Graph draws its lanes with box-drawing characters. When Herdr pane graphics are enabled, it draws antialiased curves instead:

```toml
[experimental]
kitty_graphics = true
```

This needs a terminal that supports the Kitty graphics protocol, such as Ghostty, Kitty, or WezTerm. Without it, the Graph keeps using characters.

<table>
  <tr>
    <th width="50%"><code>kitty_graphics = true</code></th>
    <th width="50%"><code>kitty_graphics = false</code></th>
  </tr>
  <tr>
    <td><img src="assets/kitty_graphics_true.png" alt="Graph with Kitty graphics enabled" width="100%"></td>
    <td><img src="assets/kitty_graphics_false.png" alt="Graph with Kitty graphics disabled" width="100%"></td>
  </tr>
  <tr>
    <td>Antialiased curves</td>
    <td>Box-drawing characters</td>
  </tr>
</table>

## Keyboard shortcuts

Hold `Alt` to see all keyboard shortcuts.

## Development

Install [just](https://just.systems/man/en/installation.html), then run:

```sh
just link   # Build, link the plugin, and bind prefix+u
just check  # Check formatting, Clippy, and tests
```

Run `just link` again after changing Rust to rebuild the linked binary.

## Contributing

[Contribution guide](CONTRIBUTING.md)

## Changelog

[Changelog](CHANGELOG.md)

## License

[MIT](LICENSE)
