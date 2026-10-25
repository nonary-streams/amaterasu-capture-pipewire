# Wayland protocol descriptions

These are upstream protocol descriptions, used by wayland-scanner to build
Amaterasu's independent native client. They contain no Vibeshine implementation.

- `zkde-screencast-unstable-v1.xml`: KDE plasma-wayland-protocols,
  commit `4c015e90ae6c88f2ffa766e899387ef431eade49`, LGPL-2.1-or-later.
  https://invent.kde.org/libraries/plasma-wayland-protocols
- `xdg-output-unstable-v1.xml`: wayland-protocols; its copyright and MIT license
  are embedded in the file. https://gitlab.freedesktop.org/wayland/wayland-protocols

KDE's ScreenCast interface is compositor-specific and explicitly unstable.
The optional build feature must be qualified against the installed KWin version.
