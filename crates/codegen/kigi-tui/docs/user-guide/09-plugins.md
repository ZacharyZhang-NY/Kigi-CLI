# Plugins

A plugin bundles skills, slash commands, agents, hooks, MCP server configurations, and LSP server configurations into one installable unit.

---

## What a plugin contains

A plugin is a directory that holds any combination of these components:

- **Skills** -- a `skills/` directory of SKILL.md files
- **Slash commands** -- a `commands/` directory of command files
- **Agents** -- an `agents/` directory of agent definitions
- **Hooks** -- a `hooks/hooks.json` file of lifecycle hooks. Plugin hooks also receive `KIGI_PLUGIN_ROOT` and `KIGI_PLUGIN_DATA` (see the [Hooks guide](10-hooks.md) for every environment variable passed to hooks).
- **MCP servers** -- a `.mcp.json` file of server configurations
- **LSP servers** -- a `.lsp.json` file of language server configurations

If a plugin includes a `plugin.json` manifest, the manifest can override paths or add metadata; otherwise components load from the convention directories. The manifest is optional: without one, Grok discovers the components above from their standard directories.

For example, a `team-tools` plugin might include a deploy skill, a code-review agent, pre-commit hooks, and a Linear MCP server. Install them together in one step.

## Environment variables in plugin hooks

Plugin hooks receive two environment variables beyond the standard ones set for every hook:

| Variable             | Description |
|----------------------|-------------|
| `KIGI_PLUGIN_ROOT`   | Absolute path to the plugin's installed directory. |
| `KIGI_PLUGIN_DATA`   | Absolute path to the plugin's writable data directory, for plugin state, caches, and logs. |

Grok sets these values and overrides any value you declare for the same key in the hook JSON's `env` map. (Grok also sets the `CLAUDE_PLUGIN_ROOT` and `CLAUDE_PLUGIN_DATA` aliases for compatibility.) See the [Hooks guide](10-hooks.md) for every environment variable passed to hooks.

---

## Plugin locations

Grok discovers plugins from these locations, in priority order:

| Location | Scope | Trust |
|----------|-------|-------|
| `_meta.pluginDirs` (`session/new` / `session/load`) | Session -- loaded for that session only | Trusted automatically |
| `--plugin-dir` (CLI flag, `grok agent`) | Process -- loaded for that agent process only | Trusted automatically |
| `.kigi/plugins/` | Project -- shared with the team through version control | Requires trust |
| `~/.kigi/plugins/` | User -- personal plugins for every project | Trusted automatically |
| `[plugins].paths` (config) | Custom directories you add in `config.toml` | Depends on location |

Grok also reads the `.claude/plugins/` equivalents for compatibility. When two plugins share a name, the higher-priority location wins.

The Agent SDKs load per-session plugins through `GrokOptions.plugins`, which arrives as `_meta.pluginDirs` on `session/new` and `session/load`; because the caller controls the directory, these plugins are always trusted -- their hooks and MCP servers activate without a prompt, and they never persist beyond the session. The `--plugin-dir` flag is the process-wide equivalent for direct CLI use (repeatable: `grok agent --no-leader --plugin-dir A --plugin-dir B stdio`); it applies to dedicated agent processes only and is ignored in leader mode (the shared leader discovers its own plugins).

---

## Manage plugins in the TUI

### Open the modal

| Action | Opens |
|--------|-------|
| `Ctrl+L` (from any pane; **non–VS Code family**) | Plugins tab |
| `/plugins` (any terminal; **required on VS Code family**) | Plugins tab |

The modal has four tabs: **Hooks**, **Plugins**, **Skills**, and **MCP Servers**. Switch tabs with `Tab` (forward) or `Shift+Tab` (backward). The `/hooks`, `/plugins`, `/skills`, and `/mcps` commands each open the modal on the matching tab.

### Plugins tab

Press `Enter` to expand a plugin row and show its details:

- **Name** and **version**
- **Scope** -- `cli`, `project`, `user`, or `custom path`
- **Skills** -- names or count
- **Agents** -- names or count
- **Hooks** -- count
- **MCP servers** -- count (or `blocked` when the plugin is not trusted)
- **Description** and **path**

Use these keys in the Plugins tab:

| Key | Action |
|-----|--------|
| `r` | Reload all plugins |
| `a` | Add a plugin from `owner/repo`, a URL, or a local path |
| `Space` | Enable or disable the selected plugin |
| `x` | Uninstall the selected plugin |
| `f` | Filter by status (all, enabled, or disabled) |
| `Enter` | Expand or collapse plugin details |
| `/` | Search plugins by name |

### Plugin commands

```bash
grok plugin list [--json] [--available]   # List installed plugins (--available requires --json)
grok plugin install <source> --trust      # Git URL, GitHub shorthand (user/repo), or local path
grok plugin uninstall <name> [--confirm] [--keep-data]   # Aliases: rm, remove
grok plugin update [<name>]               # Omit the name to update all plugins
grok plugin enable <name>
grok plugin disable <name>
grok plugin details <name>                # Show the plugin's component inventory
grok plugin validate [<path>]             # Validate plugin.json (default: current directory)
grok plugin tag [<path>] [--push] [--force] [--dry-run]   # Tag a release from the manifest version
```

Run `grok plugin install <source>` without `--trust` and Grok prints the source and warns that installing will activate the plugin's hooks, MCP servers, and skills, then stops without installing. Add `--trust` to install it.

The `<source>` argument accepts:

- `user/repo` -- GitHub shorthand
- `user/repo@v1.0` -- pinned to a ref
- `user/repo#subdir` -- subdirectory within the repo
- `https://github.com/user/repo.git` -- full URL
- `git@github.com:user/repo.git` -- SSH
- `./local-dir` or `/absolute/path` -- local directory

### Hide the plugins UI

To hide the hooks and plugins UI — the `/hooks` and `/plugins` commands and the scrollback annotations — set this in `~/.kigi/pager.toml`:

```toml
disable_plugins = true
```

---

## Trust model

Enabling a plugin loads its skills, slash commands, and agents. Trust is separate and controls whether a plugin's code runs: even for an enabled plugin, its hooks, MCP servers, and LSP servers stay inactive until you trust it. This prevents an untrusted repository from running code on your machine.

Grok trusts plugins from `~/.kigi/plugins/` automatically. Project plugins in `.kigi/plugins/` require explicit trust. To trust a plugin, install it with `--trust`:

```bash
grok plugin install <source> --trust
```

---

## Inspect plugins

Run `grok inspect` to see every discovered plugin and what it provides:

```bash
grok inspect          # Show plugins with their skills, agents, hooks, and MCP servers
grok inspect --json   # Emit machine-readable JSON
```

Plugin-provided components appear in their sections (Skills, Agents, MCP Servers, and so on) with a `plugin: <name>` label, so you can see where each component originates.

---

## General keyboard shortcuts

These keys work across every tab in the modal:

| Key | Action |
|-----|--------|
| `Tab` | Next tab |
| `Shift+Tab` | Previous tab |
| `j` / down-arrow | Move selection down |
| `k` / up-arrow | Move selection up |
| `Enter` | Expand or collapse the selected item |
| `/` | Search the current tab by name |
| `Esc` | Clear the search, or close the modal |

Some actions, such as uninstalling a plugin, ask for confirmation. Press `y` to confirm or `Esc` to cancel.
