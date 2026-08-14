# claude-usage-tray

[![GitHub Release](https://img.shields.io/github/v/release/patrickdappollonio/claude-usage-tray)](https://github.com/patrickdappollonio/claude-usage-tray/releases/latest) [![GitHub Downloads (all assets, all releases)](https://img.shields.io/github/downloads/patrickdappollonio/claude-usage-tray/total)](https://github.com/patrickdappollonio/claude-usage-tray/releases/latest) [![NPM Version](https://img.shields.io/npm/v/%40patrickdappollonio%2Fclaude-usage-tray)](https://www.npmjs.com/package/@patrickdappollonio/claude-usage-tray) [![CI](https://img.shields.io/github/actions/workflow/status/patrickdappollonio/claude-usage-tray/ci.yml?branch=main&label=ci)](https://github.com/patrickdappollonio/claude-usage-tray/actions/workflows/ci.yml) [![GitHub License](https://img.shields.io/github/license/patrickdappollonio/claude-usage-tray)](LICENSE)

Know how much Claude Code you have left, without asking. **claude-usage-tray** puts a tiny gauge in your Linux system tray showing your 5-hour session usage and your weekly usage, with the times they reset.

No window. No dashboard. Just an icon that changes color as you get closer to the limit, and a menu when you want the details.

<!-- screenshot -->

### Why you might want this

Claude Code already knows your usage. It just doesn't tell you until you go looking, and by then you are usually mid-thought on something important.

This puts the number where you can glance at it. Green means you have room. Red means wrap it up. If you would rather be told than reminded, it can also notify you as you cross 50%, 75%, 90%, 99% and 100%.

### It never talks to Anthropic

This is the part worth reading slowly.

The tray never reads your Claude credentials, never touches your OAuth tokens, and never contacts Anthropic at all. There is no account to create and nothing to log into.

Here is the whole data flow. Claude Code already computes your usage numbers and hands them to whatever you have configured as your statusline. One command wires this tool into that statusline, where it quietly copies those numbers to a small file on your machine. The tray reads that file. That's it.

Your own statusline keeps working exactly as before. If the tray ever fails to write its file, your statusline still prints what it always printed, because the piece that runs inside Claude Code is built to stay out of the way and never fail loudly.

The only network request the program can make is an optional update check, once a day, against the GitHub releases API. It is anonymous, it sends nothing but the program name and version, and it only reads the latest release tag. Turn it off in `Settings > Check for updates` and the program makes zero network requests, ever.

### Install

Every release ships statically linked binaries for x86_64 and arm64. No runtime dependencies, no glibc version to match.

**Homebrew (Linuxbrew):**

```bash
brew install patrickdappollonio/tap/claude-usage-tray
```

**npm:**

```bash
npm install -g @patrickdappollonio/claude-usage-tray
```

The npm package bundles the same prebuilt binaries and is Linux only. For a one-off run, use `npx -y @patrickdappollonio/claude-usage-tray`.

**Debian, Ubuntu and derivatives:**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.deb
sudo dpkg -i claude-usage-tray_<version>_linux_<arch>.deb
```

**Fedora, RHEL, openSUSE:**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.rpm
sudo dnf install ./claude-usage-tray_<version>_linux_<arch>.rpm
```

**Arch Linux:**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.pkg.tar.zst
sudo pacman -U claude-usage-tray_<version>_linux_<arch>.pkg.tar.zst
```

**Alpine:**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.apk
sudo apk add --allow-untrusted claude-usage-tray_<version>_linux_<arch>.apk
```

**Plain tarball:**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.tar.gz
tar xzf claude-usage-tray_<version>_linux_<arch>.tar.gz
sudo install -m 0755 claude-usage-tray /usr/local/bin/claude-usage-tray
```

Replace `<version>` with the release you want (for example `0.1.0`) and `<arch>` with `amd64` or `arm64`. Every release also carries a `checksums.txt` with SHA-256 sums for each file. Grab any of these from the [releases page](https://github.com/patrickdappollonio/claude-usage-tray/releases/latest).

### Getting started

Two commands and you're done:

```bash
claude-usage-tray hook install   # let Claude Code share its usage numbers
claude-usage-tray               # run the tray
```

`hook install` edits your Claude Code `settings.json` and nothing else. If you already have a statusline, yours is wrapped rather than replaced, so it keeps running and printing exactly what it did before. If you had no statusline, you still don't get one, because the tray doesn't add anything of its own. Your original file is backed up the first time.

Then start a Claude Code session and the numbers show up.

Two more commands, for when you need them:

```bash
claude-usage-tray hook status      # what's wired up, and how fresh the data is
claude-usage-tray hook uninstall   # put things back exactly as they were
```

If you skip the install step, the tray notices. It shows a gray icon and offers an **Install hook** button right in the menu, which does the same thing as the command above.

One small note: the hook records the full path of the binary you ran it from. If you move the binary, run `hook install` again. `hook status` will tell you when the paths no longer match.

### Reading the icon

The icon is a small gauge, and it tells you two things at once.

- **The outer arc** is your 5-hour session, sweeping clockwise as you use it up.
- **The dot in the middle** is your weekly usage.
- **The color** is the warning: green under 50%, yellow under 75%, orange under 90%, red at 90% and above. Both parts use the same bands.
- **A question mark in the middle** means the numbers haven't been updated recently. You still see the last known reading, it just might not be current.
- **A gray icon** means there's no data yet, usually because the hook isn't installed.

Left-click the icon for a plain summary, something like "You've used 32% of your 5-hour session (resets at 03:50) and 33% of your weekly limit (resets Tue 09:00)." It appears briefly and disappears on its own.

Stale doesn't mean wrong, by the way. Reset times are real timestamps, so the countdowns stay accurate on their own. When a 5-hour window rolls over, the session percentage drops back to 0% even if nothing has reported in hours.

### Settings

Everything configurable lives in the `Settings` submenu. Changes apply immediately, no restart.

- **Launch at login.** A checkbox. It writes a standard autostart entry, which KDE, GNOME, XFCE, LXQt, Cinnamon and MATE all honor.
- **Notifications.** One checkbox per threshold (50%, 75%, 90%, 99%, 100%), plus one for when your quota resets. All on by default, all optional. You get one notification per crossing, the highest one only, and restarting the tray never counts as a crossing.
- **Refresh interval.** How often the tray re-reads the numbers: 5, 15, 30 or 60 seconds.
- **Icon style.** Color, or monochrome for panels where four colors are more noise than signal. Monochrome auto follows your desktop's light or dark preference and repaints live when you switch. There's also a manual dark and light option, named after your desktop rather than the icon, so pick the one matching your panel. In monochrome the arc length carries the signal instead of the color.
- **Check for updates.** On by default, and the only setting that permits a network request. When a newer version exists, one row appears in the menu; clicking it opens the release page in your browser. Nothing is ever downloaded or installed for you. Uncheck it and the checks stop.

Settings are saved to `~/.config/claude-usage-tray/config.toml`. If the tray can't write there, the affected menu entries appear grayed out instead of pretending to accept your click.

### Desktop compatibility

The icon uses StatusNotifierItem, the freedesktop tray standard.

- **Works out of the box:** KDE Plasma, XFCE 4.16+, LXQt, Cinnamon, MATE.
- **GNOME:** needs the [AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/) extension. Without it GNOME has no tray to draw into, and no icon will appear. The tray tells you and exits rather than running invisibly.
- **Notifications** use the standard desktop notification service, which every desktop above ships by default, GNOME included, no extension needed.

### What it can't do

Worth knowing before you install.

The numbers only move forward while Claude Code is running a session, because that's when it reports them. When nothing is running, the icon goes stale and grows a question mark, but it keeps showing the last reading and the countdowns stay correct. "Check for new data" in the menu re-reads the file, but it can't make Claude Code report sooner.

You need Claude Code 2.1.80 or newer, which is the version that started including usage in the statusline data.

It also only works on Pro and Max subscriptions logged in the normal way. API-key billing doesn't include usage numbers, so there's nothing to show.

### License

MIT. See [LICENSE](LICENSE).
