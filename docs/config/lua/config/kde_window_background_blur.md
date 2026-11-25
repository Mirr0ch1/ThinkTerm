---
tags:
  - appearance
---
# `window_background_blur = false`

`kde_window_background_blur` is the older name for this setting and still
works as an alias.

{{since('nightly')}}

When combined with `window_background_opacity`, asks the desktop compositor to
blur what is behind the window. The blur is done by the compositor, so
ThinkTerm does not capture the screen or add an extra rendering pass.

```lua
config.window_background_opacity = 0.8
config.window_background_blur = true
```

On Wayland, ThinkTerm uses the standard
[`ext-background-effect-v1`](https://wayland.app/protocols/ext-background-effect-v1)
protocol. It is available on:

* GNOME 51 and later, on Wayland;
* KDE Plasma 6.7 and later;
* Hyprland 0.57.2 and later;
* niri 26.04 and later;
* COSMIC 1.11 and later.

On KDE Plasma 6.6 and earlier ThinkTerm falls back to KWin's older
`org_kde_kwin_blur` protocol.

On X11, only KWin implements a per-window blur request, through
`_KDE_NET_WM_BLUR_BEHIND_REGION`; ThinkTerm sets it on every X11 window and
other compositors ignore it.

GNOME 50 and earlier, GNOME X11, Sway, and Xfce's Xfwm4 have no supported
per-window blur. On those desktops ThinkTerm still makes the window
see-through, but the content behind it stays sharp.

See also [macos_window_background_blur](macos_window_background_blur.md) and
[win32_system_backdrop](win32_system_backdrop.md).
