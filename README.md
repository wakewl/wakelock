# wakelock

A lock screen for wlroots compositors such as [dwl](https://github.com/joeyshi12/dwl), written
in Rust with GTK 4 (through Relm4) and gtk4-session-lock (ext-session-lock-v1).

```sh
wakelock [--pam-service NAME] [--pam-dir DIR]
```

`wakelock` returns once the compositor has confirmed the lock, so swayidle can lock before the
machine sleeps, and exits 1 if the session is not locked within 5 seconds. Running it while the
session is locked does nothing. The password is checked against the `login` PAM service;
`--pam-service` and `--pam-dir` (a private PAM config directory) are mainly for testing.

The lock screen runs under a small supervisor that starts it again if it crashes, since the
compositor keeps the session locked with nothing to unlock it when the lock client dies.

With swayidle (`~/.config/swayidle/config`):

```
lock 'wakelock'
before-sleep 'loginctl lock-session'
```

## Installing

Needs Rust, `gtk4`, `gtk4-layer-shell` (which provides gtk4-session-lock) and `pam`:

```sh
cargo install --locked --git https://github.com/joeyshi12/wakelock
```

This installs to `~/.cargo/bin` (`--path .` installs a local checkout). At runtime it uses `grim`
for the background.

## Colours

`$XDG_CONFIG_HOME/wakelock/colors.css` sets the palette with `@define-color NAME #rrggbb;` for
`bg`, `bg_alt`, `accent`, `fg`, `fg_muted`, `urgent` and `shadow`. Sizes are in `src/style.css`.
