# Hook Examples

Sample hooks for Grok. Copy to `~/.kigi/hooks/` to enable globally, or to `<project>/.kigi/hooks/` for project-scoped hooks (requires `/hooks-trust`).

## Available Examples

### 1. Safe Shell Guard (`safe-shell.json`)

**Type:** blocking (`PreToolUse`)

Denies obviously destructive shell commands before they execute:
- `rm -rf /`, `sudo rm -rf`, `mkfs`, `dd` to devices, fork bombs

**Install:**
```sh
mkdir -p ~/.kigi/hooks/bin
cp examples/hooks/safe-shell.json ~/.kigi/hooks/
cp examples/hooks/bin/safe-shell-guard.sh ~/.kigi/hooks/bin/
chmod +x ~/.kigi/hooks/bin/safe-shell-guard.sh
```

### 2. No Recursive Grep (`no-recursive-grep.json`)

**Type:** blocking (`PreToolUse`)

Denies recursive `grep` invocations in the shell before they execute:
- `grep -r`, `grep -R`, `grep --recursive`, `grep --dereference-recursive`,
  `grep -d recurse`, clustered flags (`grep -rn`, `grep -nri`), and `rgrep`

Recursive grep walks an entire directory tree into memory and can OOM-kill the
agent process on large repos. The system prompt already steers the model away from
this, but a prompt is advisory — this hook makes it a hard, deterministic block.
Point the model at the dedicated search tool (ripgrep-backed) instead.

It is careful to avoid false positives: `ls -R | grep foo` (the `-R` belongs to
`ls`), `grep -e -r file` (`-r` is the pattern), and `grep -- -r file` are all
allowed.

**Install:**
```sh
mkdir -p ~/.kigi/hooks/bin
cp examples/hooks/no-recursive-grep.json ~/.kigi/hooks/
cp examples/hooks/bin/no-recursive-grep-guard.py ~/.kigi/hooks/bin/
chmod +x ~/.kigi/hooks/bin/no-recursive-grep-guard.py
```
(Requires `python3` on `PATH`.)

### 3. Session Audit Log (`session-log.json`)

**Type:** passive (`SessionStart` + `SessionEnd`)

Appends session metadata to `~/.kigi/session-audit.log` — event, session ID, cwd, timestamp.

**Install:**
```sh
mkdir -p ~/.kigi/hooks/bin
cp examples/hooks/session-log.json ~/.kigi/hooks/
cp examples/hooks/bin/session-log.sh ~/.kigi/hooks/bin/
chmod +x ~/.kigi/hooks/bin/session-log.sh
```

### 4. Tool Activity Logger (`tool-logger.json`)

**Type:** passive (`PreToolUse` + `PostToolUse`)

Logs all tool calls to `~/.kigi/tool-activity.log` — tool name, event type, effective tool name, backgrounded status.

**Install:**
```sh
mkdir -p ~/.kigi/hooks/bin
cp examples/hooks/tool-logger.json ~/.kigi/hooks/
cp examples/hooks/bin/tool-logger.sh ~/.kigi/hooks/bin/
chmod +x ~/.kigi/hooks/bin/tool-logger.sh
```

## Format

Hook files use the Claude-compatible JSON format:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/check.sh", "timeout": 5 }
        ]
      }
    ]
  }
}
```

- **Event names:** `SessionStart`, `PreToolUse`, `PostToolUse`, `SessionEnd`
- **Matcher:** regex on tool name. Claude names like `Bash`, `Read`, `Edit` are auto-expanded to also match Grok names (`run_terminal_cmd`, `read_file`, `search_replace`)
- **Timeout:** in seconds (default: 5)
- **Command:** path to script (relative to hook file directory) or inline shell command

## Script Contract

Scripts receive the hook event envelope as JSON on **stdin** and should write a response to **stdout**:

**For blocking hooks (`PreToolUse`):**
```json
{"decision":"allow"}
```
or
```json
{"decision":"deny","reason":"Explanation for the user"}
```

**Exit codes:** `0` = allow, `2` = deny, other = fail-open.

**For passive hooks:** stdout is informational only. Exit `0` for success.

## Uninstall

Remove the JSON file from `~/.kigi/hooks/`. The hook stops running on the next session.