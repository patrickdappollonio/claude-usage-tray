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
  -> claude-usage-tray statusline  (this same binary, configured as your
     statusLine.command) writes that JSON verbatim to
     ~/.claude/usage-tray-statusline.json (atomic: temp file + rename),
     for repaints carrying usage data
  -> claude-usage-tray reads that cache file every few seconds and
     renders the tray icon, tooltip, and menu from it
```

There is **no shell script and no `jq`** anywhere in this path: the binary is
both the transport and the parser.

**Cache contract (v2).** The cache file at
`${CLAUDE_CONFIG_DIR:-~/.claude}/usage-tray-statusline.json` is a byte-for-byte
copy of the JSON Claude Code sends its statusline command on stdin — the whole
document, `model`, `workspace`, `cost` and all — for repaints that carry
usage data. The tray reads `rate_limits.five_hour` and `rate_limits.seven_day`
(`used_percentage`, `resets_at`) out of it and ignores everything else. A
repaint whose `rate_limits` is absent or null does not overwrite a cache that
already has real data (see [below](#the-statusline-subcommand)), so a
session's pre-first-turn statusline paint can't clobber the previous
session's numbers.

There is no timestamp *inside* the file: **freshness comes from the file's
mtime**. Older than 10 minutes and the tray treats the data as stale (dimmed
icon, `⚠ Stale since HH:MM`).

Earlier versions used a different file (`usage-tray-cache.json`) written by a
shell snippet. `claude-usage-tray hook install` deletes that file and strips
the old injected shell block out of your statusline script automatically; you
do not need to clean anything up by hand.

## No network calls, no credentials — ever

This tray makes **zero network requests** and **never reads or touches your
Claude credentials** (`.credentials.json`, OAuth tokens, or any cookie). It
only reads a local JSON file — the data Claude Code itself already hands to
your statusline on stdin. This matters because Anthropic's January 2026 Terms of
Service update prohibits using subscription OAuth outside Claude Code, and
tools that impersonate a browser or spoof the Claude Code user agent to poll
usage over the network have gotten accounts auto-banned. This tool never
does that class of thing, by construction: there is no HTTP client in the
dependency tree at all.

## Installation

Every release ships statically linked binaries for **x86\_64** and **arm64** —
no runtime dependencies, no glibc version to match. Pick whichever of these
suits your system; replace `<version>` with the release you want (for example
`0.1.0`) and `<arch>` with `amd64` or `arm64`.

### Debian, Ubuntu and derivatives

```sh
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.deb
sudo dpkg -i claude-usage-tray_<version>_linux_<arch>.deb
```

### Fedora, RHEL, openSUSE

```sh
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.rpm
sudo rpm -i claude-usage-tray_<version>_linux_<arch>.rpm
# or, to let your package manager resolve it:
sudo dnf install ./claude-usage-tray_<version>_linux_<arch>.rpm
```

### Arch Linux

```sh
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.pkg.tar.zst
sudo pacman -U claude-usage-tray_<version>_linux_<arch>.pkg.tar.zst
```

### Alpine

```sh
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.apk
sudo apk add --allow-untrusted claude-usage-tray_<version>_linux_<arch>.apk
```

### npm

```sh
npm install -g @patrickdappollonio/claude-usage-tray
```

Or run it without installing, with `npx @patrickdappollonio/claude-usage-tray`.
The npm package bundles the same prebuilt binaries and is Linux-only.

### Homebrew / Linuxbrew

Once releases are published, the tap provides it:

```sh
brew install patrickdappollonio/tap/claude-usage-tray
```

### Plain tarball

If none of the above fit, grab the archive and drop the binary somewhere on
your `PATH`:

```sh
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.tar.gz
tar xzf claude-usage-tray_<version>_linux_<arch>.tar.gz
sudo install -m 0755 claude-usage-tray /usr/local/bin/claude-usage-tray
```

Each release also carries a `checksums.txt` with SHA-256 sums for every asset.

## Run

```sh
claude-usage-tray               # run the tray
claude-usage-tray hook install  # wire up the Claude Code statusline
```

The binary has three modes, dispatched from its first argument:

| Command | What it does |
| --- | --- |
| *(no arguments)* | Runs the tray. |
| `statusline [--exec CMD]` | Claude Code's statusline command: caches the stdin JSON, optionally running `CMD` and passing its output through. See [below](#the-statusline-subcommand). |
| `hook install` / `hook uninstall` / `hook status` | Manages the `settings.json` entry. See [below](#installing-the-statusline-hook). |

Anything else prints a short usage message and exits 2. Because the tray
binary is also its own installer and its own statusline command, `hook
install` records an **absolute path** — reinstall (or re-run `hook install`)
after moving the binary; `hook status` says so when the recorded path and the
running one differ.

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
- **Notifications** — a submenu with one checkbox per usage threshold
  (`At 50%` … `At 100%`) plus `When quota resets`. See
  [Notifications](#notifications) below for what each one does. Changes take
  effect immediately; no restart.
- **Refresh interval** — 5 s / 15 s / 30 s / 60 s. Changes take effect
  immediately; no restart.
- **Icon style** — `Color` / `Monochrome (auto)` / `Monochrome dark` /
  `Monochrome light`. See [Icon style](#icon-style) below. Changes take
  effect immediately; no restart.

**Grayed-out entries**: the tray checks, each time you open the menu,
whether it can actually act on these settings. If
`~/.config/claude-usage-tray/` cannot be created or written to (read-only
home, full disk, wrong ownership), the refresh-interval options and the
notification checkboxes and the icon-style options are shown disabled
rather than accepting a click
that could not be saved. If `~/.config/autostart/` is unavailable, `Launch
at login` is disabled the same way. Because the check runs on every menu
open, fixing the permissions un-grays the entries without restarting the
tray.

Settings are saved to `~/.config/claude-usage-tray/config.toml`
(`$XDG_CONFIG_HOME` is respected if set), written atomically:

```toml
refresh_secs = 5
launch_at_login = false
notify_thresholds = [50, 75, 90, 99, 100]
notify_on_reset = true
icon_style = "color"
```

A missing or corrupt config file is not an error: the tray falls back to the
defaults above. `notify_thresholds` is the list of thresholds that are
switched **on**; unknown or out-of-range entries are dropped on load, a value
that is not a list at all falls back to the default set, and an empty list is
respected as "no threshold alerts". An `icon_style` that is missing,
misspelled or of the wrong type falls back to `"color"`.

## Installing the statusline hook

The tray is only useful once something is writing the cache file. That
something is this same binary, configured as your Claude Code statusline
command. One command installs it:

```sh
claude-usage-tray hook install
```

That edits `${CLAUDE_CONFIG_DIR:-~/.claude}/settings.json` and nothing else.
Concretely:

- **No statusline configured yet** → `statusLine.command` becomes
  `"/abs/path/claude-usage-tray statusline"`. Your statusline then prints
  nothing (it printed nothing before, and the tray deliberately does not add
  anything of its own).
- **You already have a statusline** → yours is wrapped, not replaced:
  `"/abs/path/claude-usage-tray statusline --exec '~/.claude/statusline-command.sh'"`.
  Your command still receives the same JSON on stdin and its output still goes
  to the statusline, byte for byte.
- **Already installed** → the entry is refreshed in place (handy after moving
  or rebuilding the binary). It is never wrapped twice: an existing entry is
  recognized by its `statusline` argument, whatever the binary is called.

Every other key in `settings.json` is preserved (it is a JSON
read-modify-write, pretty-printed and written atomically). The first install
copies the original file to `settings.json.bak-usage-tray`; later installs
keep that first backup rather than overwriting it with an already-modified
copy.

Then start a new Claude Code session (or wait for the statusline to refresh)
and the tray picks up data.

### Checking and removing it

```sh
claude-usage-tray hook status      # what is wired up, and how fresh the cache is
claude-usage-tray hook uninstall   # put your original command back
```

`uninstall` restores the command that was wrapped in `--exec`, or removes the
`statusLine` key entirely if there was nothing to restore, and deletes the
cache file. A `statusLine.command` that isn't ours is left untouched.

### The `statusline` subcommand

You can also wire it up by hand — this is all `hook install` does:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/abs/path/claude-usage-tray statusline --exec '~/.claude/statusline-command.sh'"
  }
}
```

Drop the `--exec '...'` part if you have no statusline command of your own.

The subcommand is deliberately boring: it reads stdin to EOF, and then, with
`--exec`, runs your command via `sh -c` with the same bytes on stdin and lets
its stdout through unmodified. **It always exits 0 and never prints anything
of its own**, even if the cache write fails or your command is missing or
fails — a usage tray must not be able to break your statusline.

Repaints carrying usage data are written to the cache verbatim. One
exception: a freshly started session paints its statusline once before its
first turn, with `rate_limits` absent, and that repaint is skipped rather
than clobbering a cache that already has real usage data — otherwise the
tray would flash "no data" at the start of every session. If there is no
cache yet, or the existing cache is equally empty, it writes anyway, so the
first-run experience is unaffected.

### Upgrading from the old shell hook

Nothing to do: run `claude-usage-tray hook install`. It strips the old
`# --- claude-usage-tray hook … # --- end claude-usage-tray hook ---` block
out of your statusline script (backing the script up as
`<script>.bak-usage-tray` first) and deletes the obsolete
`usage-tray-cache.json`.

## Icon legend

- **Outer arc**: sweeps clockwise with 5-hour session usage percentage.
- **Inner dot**: 7-day weekly usage percentage, same color bands.
- **Color bands**: green below 60%, amber below 80%, orange below 95%, red
  at 95% and above.
- **Dimmed icon**: cache file is older than 10 minutes (stale) — usage may
  have changed since the last time Claude Code ran.
- **Gray icon**: no cache file found, or it couldn't be parsed. The menu then
  reads `⚠ Hook not installed — no data` and offers a clickable **Install
  hook** item that does the same thing as `claude-usage-tray hook install`,
  followed by a `Hook installed — data appears next time Claude Code
  refreshes` toast. That item is only there while there is no data.

## Icon style

`Settings ▸ Icon style` chooses between the colored gauge and a flat
monochrome one, for panels and themes where four severity colors are more
noise than signal. The setting is saved as `icon_style` in `config.toml` and
applies the moment you pick it.

| Menu option | `config.toml` | What you get |
| --- | --- | --- |
| `Color` (default) | `"color"` | The banded gauge described above. |
| `Monochrome (auto)` | `"mono-auto"` | One flat color, following your desktop's light/dark preference. |
| `Monochrome dark` | `"mono-dark"` | One flat color, pinned to "my desktop is dark". |
| `Monochrome light` | `"mono-light"` | One flat color, pinned to "my desktop is light". |

In monochrome mode the ring, the session arc and the weekly dot are all drawn
in the same color, and the **length of the arc sweep alone** carries the usage
signal — there is no green/amber/red to read. Everything else behaves exactly
as in color mode: a stale cache still dims the icon, and a missing cache still
renders the gray "no data" ring and dot.

**The names describe your UI, not the icon.** `Monochrome dark` means "my
desktop is dark", so it paints a **near-white** icon; `Monochrome light` means
"my desktop is light" and paints a **near-black** one. Pick the one that
matches your panel; if the icon comes out invisible, you have picked the other
one.

`Monochrome (auto)` reads your preference from the XDG Desktop Portal
(`org.freedesktop.portal.Settings`, namespace `org.freedesktop.appearance`,
key `color-scheme`), which is the same setting KDE, GNOME and friends
publish for every other app: `1` means "prefer dark" (light icon), `0` and
`2` mean no preference or "prefer light" (dark icon). The tray also
subscribes to the portal's change signal, so flipping your desktop between
light and dark repaints the icon live, without a restart. If no portal is
running — or the lookup fails for any other reason — auto assumes a **dark
desktop** and draws the light icon; pin `Monochrome light` if that is wrong
for your setup.

## Notifications

Desktop notifications fire as your 5-hour session usage climbs past
**50%, 75%, 90%, 99% and 100%**. All five are on by default and each one can
be switched off individually under `Settings ▸ Notifications`.

- **Urgency**: 50% and 75% are normal priority; 90%, 99% and 100% are
  critical, so they stay on screen on desktops that treat critical
  notifications that way.
- **Once per crossing**: a threshold re-arms only when the 5-hour window
  resets or usage drops back below it, so you are not alerted on every poll
  tick.
- **Only the highest**: if usage jumps from 10% to 99% between two reads,
  you get one notification (99%), not four.
- **Toggling is not retroactive**: switching a threshold back on while usage
  is already past it stays quiet until the next genuine crossing.

**When quota resets** (also on by default, same submenu) is a separate
notification — `Session quota reset — fresh 5-hour window`, normal priority
— fired when the session window's reset time arrives. It comes from the
tray's own clock, not from the cache, so it is on time even when no Claude
Code session is running and nothing has refreshed the cache for hours. It
fires at most once per reset time, and never if the cache has no reset time
to work from.

These notifications persist in your notification history; the "Check for new
data" toast described above is transient.

## Environment variables

- `CLAUDE_TRAY_POLL_SECS` — how often (in seconds) the tray re-reads the
  cache file. **Takes precedence over the configured refresh interval** when
  it is set to a positive integer; anything else (unset, `0`, garbage) is
  ignored and the config file wins. While the override is in effect the
  radio group still saves your choice to the config file — it just doesn't
  change the running interval, and the submenu says so. Removing the
  variable and restarting makes the saved choice take effect.
- `CLAUDE_CONFIG_DIR` — Claude Code's own config-directory override, honoured
  here too: it decides where `hook install` edits `settings.json` and where
  the cache file (`$CLAUDE_CONFIG_DIR/usage-tray-statusline.json`) is written
  and read. Defaults to `~/.claude`.

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

MIT — see [LICENSE](LICENSE).
