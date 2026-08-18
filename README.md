# claude-usage-tray

[![GitHub Release](https://img.shields.io/github/v/release/patrickdappollonio/claude-usage-tray)](https://github.com/patrickdappollonio/claude-usage-tray/releases/latest) [![GitHub Downloads (all assets, all releases)](https://img.shields.io/github/downloads/patrickdappollonio/claude-usage-tray/total)](https://github.com/patrickdappollonio/claude-usage-tray/releases/latest) [![NPM Version](https://img.shields.io/npm/v/%40patrickdappollonio%2Fclaude-usage-tray)](https://www.npmjs.com/package/@patrickdappollonio/claude-usage-tray) ![NPM Downloads](https://img.shields.io/npm/d18m/%40patrickdappollonio%2Fclaude-usage-tray?label=npm%20downloads) [![CI](https://img.shields.io/github/actions/workflow/status/patrickdappollonio/claude-usage-tray/ci.yml?branch=main&label=ci)](https://github.com/patrickdappollonio/claude-usage-tray/actions/workflows/ci.yml) [![GitHub License](https://img.shields.io/github/license/patrickdappollonio/claude-usage-tray)](LICENSE)

<img align="right" src="assets/states.gif" width="190" alt="The tray gauge cycling through its states">

`claude-usage-tray` is a tiny application that lives in your menu bar on macOS or your system tray on Linux, and it reports the status of your Claude quotas and sessions. The numbers come straight from the Claude CLI itself, so there are no cookies to copy and no awkward browser hacks to keep alive. It installs a small hook on the Claude CLI, and from then on, whenever you use Claude normally, fresh numbers flow to the tray and keep it current. Other tools resort to tricks like impersonating the Claude CLI, which can get an account banned, since [Anthropic disallows the practice](https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/).

**[Jump to installation &rarr;](#install)**

The app can also notify you at the moments that matter: when you cross usage thresholds you choose, and when your quotas reset, both the 5-hour window and the weekly one. All of it is configurable from the tray menu itself.

There is no window and no dashboard to keep open. Just a small gauge that changes color as you get closer to the limit, and a menu when you want the details. Green means you have room, red means wrap it up.

### 🔒 It never talks to the Claude Website or Anthropic APIs

This is the part worth reading slowly.

The tray never reads your Claude credentials, never touches your OAuth tokens, and never contacts Anthropic or the Claude website at all. There is no account to create and nothing to log into.

Here is the whole data flow. Claude Code already computes your usage numbers and hands them to whatever you have configured as your statusline. One command wires this tool into that statusline, where it quietly copies those numbers to a small file on your machine. The tray reads that file. That's it.

Your own statusline keeps working exactly as before. If the tray ever fails to write its file, your statusline still prints what it always printed, because the piece that runs inside Claude Code is built to stay out of the way and never fail loudly.

The only network request the program can make is an optional update check, once a day, against the GitHub releases API. It is anonymous, it sends nothing but the program name and version, and it only reads the latest release tag. Turn it off in `Settings > Check for updates` and the program makes zero network requests, ever.

### 🧭 Reading the icon

The icon is a small gauge, and it tells you two things at once.

- **The outer arc** is your 5-hour session, sweeping clockwise as you use it up.
- **The dot in the middle** is your weekly usage.
- **The color** is the warning: green under 50%, yellow under 75%, orange under 90%, red at 90% and above. Both parts use the same bands.
- **A question mark in the middle** means the numbers haven't been updated recently. You still see the last known reading, it just might not be current.
- **A gray icon** means there's no data yet, usually because the hook isn't installed.

On Linux, left-click the icon for a plain summary, something like "You've used 32% of your 5-hour session (resets in 3 h) and 33% of your weekly limit (resets in 2 d)." It appears briefly and disappears on its own. On macOS any click simply opens the menu, as menu bar items do, and the same numbers lead it.

Stale doesn't mean wrong, by the way. Reset times are real timestamps, so the countdowns stay accurate on their own. When a 5-hour window rolls over, the session percentage drops back to 0% even if nothing has reported in hours.

<a id="install"></a>

### 📦 Install

Every release ships binaries for x86_64 and arm64, on both Linux and macOS. The Linux ones are statically linked: no runtime dependencies, no glibc version to match. macOS additionally gets a real `.app`, which is the one to pick if you want notifications; it is a few blocks down.

**Homebrew (macOS, recommended):**

```bash
brew install --cask patrickdappollonio/tap/claude-usage-tray
```

The cask installs the full application into `/Applications`, which is the version with proper notifications, and it puts the `claude-usage-tray` command on your `PATH` too.

**Homebrew (Linuxbrew):**

```bash
brew install patrickdappollonio/tap/claude-usage-tray
```

The formula is Linux only and installs just the binary, which on Linux is the whole app. On macOS, use the cask above.

**npm:**

```bash
npm install -g @patrickdappollonio/claude-usage-tray
```

The npm package bundles the same prebuilt binaries for Linux and macOS. For a one-off run, use `npx -y @patrickdappollonio/claude-usage-tray`.

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

**Plain tarball (Linux):**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_linux_<arch>.tar.gz
tar xzf claude-usage-tray_<version>_linux_<arch>.tar.gz
sudo install -m 0755 claude-usage-tray /usr/local/bin/claude-usage-tray
```

**App bundle (macOS, recommended):**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_darwin_<arch>_app.zip
unzip claude-usage-tray_<version>_darwin_<arch>_app.zip
mv "Claude Usage Tray.app" /Applications/
open "/Applications/Claude Usage Tray.app"
```

This is the same tray, wrapped in a real macOS application, and it is the version to pick if you want notifications that actually show up. It opens straight away, with no warning to click through and no `xattr` incantation, because the macOS builds are signed with a Developer ID certificate and notarized by Apple. There is no Dock icon and no app window: it goes straight to the menu bar, exactly like the bare binary does.

The command line still works from inside the bundle. The binary lives at `/Applications/Claude Usage Tray.app/Contents/MacOS/claude-usage-tray`, so `hook install` and friends are available if you point at it, and it is worth adding to your `PATH` if you use them often:

```bash
"/Applications/Claude Usage Tray.app/Contents/MacOS/claude-usage-tray" hook install
```

**Plain tarball (macOS):**

```bash
curl -LO https://github.com/patrickdappollonio/claude-usage-tray/releases/latest/download/claude-usage-tray_<version>_darwin_<arch>.tar.gz
tar xzf claude-usage-tray_<version>_darwin_<arch>.tar.gz
sudo install -m 0755 claude-usage-tray /usr/local/bin/claude-usage-tray
```

Nothing to unquarantine here either: the macOS binaries are signed with a Developer ID certificate and notarized by Apple, so a direct download runs as it is. Homebrew and npm work exactly as they always did.

Replace `<version>` with the release you want (for example `0.1.0`) and `<arch>` with `amd64` or `arm64`. Every release also carries a `checksums.txt` with SHA-256 sums for each file. Grab any of these from the [releases page](https://github.com/patrickdappollonio/claude-usage-tray/releases/latest).

### 🚀 Getting started

Two commands and you're done:

```bash
claude-usage-tray hook install  # let Claude Code share its usage numbers
claude-usage-tray               # run the tray
```

The second command hands your terminal straight back. The tray keeps running in the background, and it survives closing that terminal, so there is nothing to leave open and no `&` to remember. Only one tray runs at a time: start a second one and it tells you so instead of putting two icons in your panel.

If you would rather watch it run, add `--foreground` and it stays attached until you stop it with Ctrl-C. That is the useful form for debugging, and it is what the launch-at-login entry uses so your session manager can supervise it properly.

`hook install` edits your Claude Code `settings.json` and nothing else. If you already have a statusline, yours is wrapped rather than replaced, so it keeps running and printing exactly what it did before. If you had no statusline, you still don't get one, because the tray doesn't add anything of its own. Your original file is backed up the first time.

Then start a Claude Code session and the numbers show up.

Two more commands, for when you need them:

```bash
claude-usage-tray hook status      # what's wired up, and how fresh the data is
claude-usage-tray hook uninstall   # put things back exactly as they were
claude-usage-tray restart          # swap the running tray for the one on disk
```

`restart` is the one to run after an upgrade. Installing a new version doesn't disturb the copy already running, so until you restart it you are still looking at the old one.

If you skip the install step, the tray notices. It shows a gray icon and offers an **Install hook** button right in the menu, which does the same thing as the command above.

One small note: the hook records the full path of the binary you ran it from. If you move the binary, run `hook install` again. `hook status` will tell you when the paths no longer match.

### ⚙️ Settings

Everything configurable lives in the `Settings` submenu. Changes apply immediately, no restart.

- **Launch at login.** A checkbox. On Linux it writes a standard autostart entry, which KDE, GNOME, XFCE, LXQt, Cinnamon and MATE all honor. On macOS it writes a LaunchAgent pointing at whichever copy you ran it from, the one inside the app bundle included; note that macOS lists LaunchAgents under Login Items as "Allow in the Background" rather than "Open at Login", but it starts at login all the same.
- **Notifications.** One checkbox per threshold (50%, 75%, 90%, 99%, 100%), plus one for when your quota resets. All on by default, all optional. You get one notification per crossing, the highest one only, and restarting the tray never counts as a crossing.
- **Refresh interval.** How often the tray re-reads the numbers: 5, 15, 30 or 60 seconds.
- **Icon style.** Color, or monochrome for panels where four colors are more noise than signal. Monochrome auto follows your desktop's light or dark preference and repaints live when you switch. There's also a manual dark and light option, named after your desktop rather than the icon, so pick the one matching your panel. In monochrome the arc length carries the signal instead of the color. Linux defaults to color; macOS defaults to monochrome, drawn as a template image the system tints to match either menu bar, since the colored gauge's dim ring gets lost against a dark menu bar.
- **Check for updates.** On by default, and the only setting that permits a network request. When a newer version exists, one row appears in the menu; clicking it opens the release page in your browser. Nothing is ever downloaded or installed for you. Once you have installed the new version through your package manager, run `claude-usage-tray restart` to switch over to it. The tray notices the swap on its own too, and offers a **Restart to update** row in the menu that does the same thing in one click. Uncheck it and the checks stop.

Settings are saved to `~/.config/claude-usage-tray/config.toml` on Linux and `~/Library/Application Support/claude-usage-tray/config.toml` on macOS. If the tray can't write there, the affected menu entries appear grayed out instead of pretending to accept your click.

### 🖥️ Desktop compatibility

On Linux the icon uses StatusNotifierItem, the freedesktop tray standard. On macOS it lives in the menu bar through the native APIs.

- **Works out of the box:** KDE Plasma, XFCE 4.16+, LXQt, Cinnamon, MATE, and macOS.
- **macOS notes:** the icon adapts to light and dark menu bars automatically. Notifications depend on how you installed it, which is what the next section is about.
- **GNOME:** needs the [AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/) extension. Without it GNOME has no tray to draw into, and no icon will appear. The tray tells you and exits rather than running invisibly.
- **Notifications** use the standard desktop notification service, which every desktop above ships by default, GNOME included, no extension needed.

### 🔔 Notifications on macOS

macOS only delivers notifications on behalf of something it can name, and a bare command line binary has no name to give. That is the whole reason the app bundle exists.

Install the `.app` and notifications go through Notification Center properly. The first time the tray has something to tell you, macOS asks whether Claude Usage Tray may send notifications; say yes once and that is the end of it. From then on the banners carry the app's name, they land in Notification Center where you can scroll back through them, and you can tune or silence them in System Settings under Notifications like any other app.

Install the bare binary instead (npm or the tarball) and everything else works exactly the same, but notifications are best effort. The tray still tries, using the only route available to an unbundled program, and on recent macOS versions that route often delivers nothing at all. Nothing breaks and nothing is logged in your face; you simply may not see the banners. If threshold alerts matter to you, use the bundle.

Launch at login is unchanged either way. The checkbox writes a LaunchAgent, which macOS lists under Login Items as "Allow in the Background" rather than "Open at Login". The bundle does not register itself for the "Open at Login" list, so do not go looking for it there.

### 🚧 What it can't do

Worth knowing before you install.

The numbers only move forward while Claude Code is running a session, because that's when it reports them. When nothing is running, the icon goes stale and grows a question mark, but it keeps showing the last reading and the countdowns stay correct. "Check for new data" in the menu re-reads the file, but it can't make Claude Code report sooner.

You need Claude Code 2.1.80 or newer, which is the version that started including usage in the statusline data.

It also only works on Pro and Max subscriptions logged in the normal way. API-key billing doesn't include usage numbers, so there's nothing to show.

### 📄 License

MIT. See [LICENSE](LICENSE).
