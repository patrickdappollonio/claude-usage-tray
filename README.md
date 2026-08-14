# claude-usage-tray

A minimal Linux tray icon (StatusNotifierItem — KDE Plasma primary, GNOME via
the AppIndicator extension) that shows your Claude Code subscription usage:
the 5-hour session percentage and the 7-day weekly percentage, with reset
times. This is a proof of concept: a status icon and a menu, nothing more.
No windows, no charts, no separate settings window (the handful of settings
live in the menu itself), no multi-account profiles.

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
loop, re-reading the cache file on an interval (see [Settings](#settings)
below) and updating the icon when the content changes. Left-click the icon or
use "Check for new data" in the menu to force an immediate re-read. "Quit"
exits cleanly.

The menu item is called "Check for new data" rather than "Refresh" on
purpose: it re-reads the local cache file, which only moves forward when
Claude Code itself runs your statusline. Clicking it cannot make Claude Code
report sooner. To tell you which of the two happened, a user-initiated
re-read pops a short, low-priority notification:

- `Updated — Session 7%, Weekly 28%` — the cache had newer data.
- `No new data — Claude Code last reported at 14:32` — the cache is
  unchanged since that time (usually: no Claude Code session is running).
- `No data — install the statusline hook` — there is no readable cache file
  at all.

These are marked transient, so they disappear on their own and do not pile
up in your notification history. Timer-driven polls never notify.

## Desktop compatibility

The tray icon uses StatusNotifierItem (SNI), the freedesktop tray protocol:

- **Works out of the box**: KDE Plasma, XFCE 4.16+, LXQt, Cinnamon, MATE.
- **GNOME**: requires the
  [AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/)
  shell extension. Without it GNOME does not implement SNI at all and **no
  icon will appear** — the process runs fine, it just has nowhere to draw.
  The tray prints an error and exits if no SNI host is available at startup.
- **Notifications** (threshold alerts and the refresh toast) use the
  freedesktop desktop-notification standard, which every one of the above
  ships by default, GNOME included, with no extension needed.

## Settings

The tray menu has a `Settings` submenu with everything that is configurable:

- **Launch at login** — a checkbox. Checking it writes an XDG autostart
  entry to `~/.config/autostart/claude-usage-tray.desktop` pointing at the
  running binary's absolute path; unchecking it deletes that file. This is
  the standard mechanism honoured by KDE, GNOME, XFCE, LXQt, Cinnamon and
  MATE alike, so it works the same everywhere. The checkbox reflects whether
  that file currently exists, so removing it by hand is picked up too. If
  the file can't be written (read-only home, for instance), an error is
  printed and the checkbox stays as it was. Note the entry records the path
  of the binary you enabled it from — if you move the binary, re-toggle the
  checkbox.
- **Refresh interval** — 5 s / 15 s / 30 s / 60 s. Changes take effect
  immediately; no restart.

Both settings are saved to `~/.config/claude-usage-tray/config.toml`
(`$XDG_CONFIG_HOME` is respected if set), written atomically:

```toml
refresh_secs = 5
launch_at_login = false
```

A missing or corrupt config file is not an error: the tray falls back to the
defaults above.

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
spammed on every poll tick. These are the only notifications that persist;
the "Check for new data" toast described above is transient.

## Environment variables

- `CLAUDE_TRAY_POLL_SECS` — how often (in seconds) the tray re-reads the
  cache file. **Takes precedence over the configured refresh interval** when
  it is set to a positive integer; anything else (unset, `0`, garbage) is
  ignored and the config file wins. While the override is in effect the
  radio group still saves your choice to the config file — it just doesn't
  change the running interval, and the submenu says so. Removing the
  variable and restarting makes the saved choice take effect.
- `CLAUDE_CONFIG_DIR` — overrides where both the statusline hook writes and
  the tray reads the cache file (`$CLAUDE_CONFIG_DIR/usage-tray-cache.json`).
  Defaults to `~/.claude`.

Effective refresh interval, highest priority first:
`CLAUDE_TRAY_POLL_SECS` → `refresh_secs` in `config.toml` → `5`.

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
