# claude-usage-tray

A minimal Linux tray icon (StatusNotifierItem — KDE Plasma primary, GNOME via
the AppIndicator extension) that shows your Claude Code subscription usage:
the 5-hour session percentage and the 7-day weekly percentage, with reset
times. This is a proof of concept: a status icon and a menu, nothing more.
No windows, no charts, no settings UI, no multi-account profiles, no
autostart packaging.

Inspired by [hamed-elfayome/Claude-Usage-Tracker](https://github.com/hamed-elfayome/Claude-Usage-Tracker)
(macOS), but built on a different, sanctioned data source — see below.

## How data flows

```
Claude Code statusline JSON (stdin, includes rate_limits since v2.1.80)
  -> statusline-hook.snippet.sh tees rate_limits into
     ~/.claude/usage-tray-cache.json (atomic write: temp file + mv)
  -> claude-usage-tray reads that cache file every few seconds and
     renders the tray icon, tooltip, and menu from it
```

The cache file contains exactly:

```json
{"written_at": 1700000000, "rate_limits": {"five_hour": {...}, "seven_day": {...}}}
```

`written_at` is a Unix timestamp (seconds); `rate_limits` is the verbatim
object Claude Code puts in the statusline JSON.

## No network calls, no credentials — ever

This tray makes **zero network requests** and **never reads or touches your
Claude credentials** (`.credentials.json`, OAuth tokens, or any cookie). It
only reads a local JSON file that Claude Code itself already writes via your
statusline script. This matters because Anthropic's January 2026 Terms of
Service update prohibits using subscription OAuth outside Claude Code, and
tools that impersonate a browser or spoof the Claude Code user agent to poll
usage over the network have gotten accounts auto-banned. This tool never
does that class of thing, by construction: there is no HTTP client in the
dependency tree at all.

## Build

```sh
cargo build --release
```

The binary is at `target/release/claude-usage-tray`.

## Run

```sh
./target/release/claude-usage-tray
```

The ksni tray service runs on its own thread; the main thread runs the poll
loop, re-reading the cache file on an interval (see `CLAUDE_TRAY_POLL_SECS`
below) and updating the icon when the content changes. Left-click the icon or
use "Refresh now" in the menu to force an immediate re-read. "Quit" exits
cleanly.

## Installing the statusline hook

The tray is only useful once something is writing the cache file. That
something is a couple of lines added to your Claude Code statusline script,
copied from `statusline-hook.snippet.sh` in this repo.

### If you already have a statusline script

Open your existing statusline script (referenced by `statusLine.command` in
`~/.claude/settings.json`) and find the line near the top that reads:

```sh
input=$(cat)
```

Paste the contents of `statusline-hook.snippet.sh` immediately after that
line, then leave the rest of your statusline script (whatever prints your
prompt) unchanged below it.

### If you don't have a statusline script yet

1. Create `~/.claude/statusline-command.sh`:

   ```sh
   #!/bin/bash
   input=$(cat)

   # paste the contents of statusline-hook.snippet.sh here

   # your normal statusline output, e.g.:
   echo "$(echo "$input" | jq -r '.model.display_name // "Claude"')"
   ```

2. Make it executable: `chmod +x ~/.claude/statusline-command.sh`

3. Point Claude Code at it in `~/.claude/settings.json`:

   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "~/.claude/statusline-command.sh"
     }
   }
   ```

4. Restart Claude Code (or start a new session) so the statusline runs.

The hook requires `jq`. If `jq` is missing, or the statusline JSON has no
`rate_limits` field, the hook is a safe no-op: it writes nothing rather than
a malformed cache file, and never breaks your statusline output.

## Icon legend

- **Outer arc**: sweeps clockwise with 5-hour session usage percentage.
- **Inner dot**: 7-day weekly usage percentage, same color bands.
- **Color bands**: green below 60%, amber below 80%, orange below 95%, red
  at 95% and above.
- **Dimmed icon**: cache file is older than 10 minutes (stale) — usage may
  have changed since the last time Claude Code ran.
- **Gray icon**: no cache file found, or it couldn't be parsed — install the
  statusline hook (see above).

## Notifications

A desktop notification fires once when session usage crosses 80% (normal
priority) and again at 95% (critical priority). Each threshold re-arms only
when the 5-hour window resets or usage drops back below it, so you won't be
spammed on every poll tick.

## Configuration

- `CLAUDE_TRAY_POLL_SECS` — how often (in seconds) the tray re-reads the
  cache file. Default: `5`.
- `CLAUDE_CONFIG_DIR` — overrides where both the statusline hook writes and
  the tray reads the cache file (`$CLAUDE_CONFIG_DIR/usage-tray-cache.json`).
  Defaults to `~/.claude`.

## Limitations

- Usage data only refreshes while Claude Code is actively running a session
  (the statusline only runs then). When no session is running, the icon
  will go stale and dim, but reset-time countdowns remain accurate
  regardless.
- Requires Claude Code >= 2.1.80 (the version that added `rate_limits` to
  the statusline JSON).
- Only works for Pro/Max subscription accounts authenticated via Claude
  Code's normal OAuth login. `rate_limits` is absent from the statusline
  JSON for API-key billing, so there is nothing for this tool to show in
  that case.

## License

No license has been chosen yet for this project.
