# Herdr plugin notes

Facts from the Herdr documentation that this plugin relies on.

## Command reference

Local development:

```sh
herdr plugin link "$PWD"
herdr plugin action list --plugin io.github.jehwanyoo.herdr-git
herdr plugin action invoke io.github.jehwanyoo.herdr-git.open
herdr plugin log list --plugin io.github.jehwanyoo.herdr-git
herdr plugin unlink io.github.jehwanyoo.herdr-git
```

`plugin link` does not run manifest build commands; build the release binary first.

Marketplace users:

```sh
herdr plugin install JeHwanYoo/herdr-git
herdr plugin install JeHwanYoo/herdr-git --ref <tag>
herdr plugin uninstall io.github.jehwanyoo.herdr-git
```

`plugin install` runs the manifest `[[build]]` commands after confirmation.

Runtime commands run with the plugin directory as their working directory. Herdr injects `HERDR_BIN_PATH`, `HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_CONTEXT_JSON`, and, when available, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, and `HERDR_PANE_ID`. Local runtime state belongs under `HERDR_PLUGIN_STATE_DIR`.

## Right-click input

Herdr shows its own pane menu on right-click. The 0.8.2 CLI reference documents `herdr pane input <pane_id> --right-click pane`, which forwards unmodified right-clicks to the application in that pane; the plugin does not set this policy, and Herdr 0.8.0 has no `pane input` command. `Alt+A` opens the Commit actions menu from the keyboard.

## Marketplace release

1. Publish the repository publicly on GitHub.
2. Keep a parseable `herdr-plugin.toml` on the default branch.
3. Add the GitHub repository topic `herdr-plugin`.
4. Verify a clean install with `herdr plugin install JeHwanYoo/herdr-git --ref <tag>`.
5. Confirm the marketplace entry after its automatic refresh, documented as every 30 minutes.

The marketplace is an automatic, unreviewed index. Forks, archived repositories, malformed manifests, and repositories without a valid manifest are excluded.

## Sources

- [Herdr Plugins](https://herdr.dev/docs/plugins/)
- [Herdr Marketplace](https://herdr.dev/docs/marketplace/)
- [Herdr CLI reference](https://herdr.dev/docs/cli-reference/)
- [Herdr configuration](https://herdr.dev/docs/configuration/)
- [Herdr config reference](https://herdr.dev/docs/config-reference/)
