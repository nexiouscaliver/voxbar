# VoxBar on Ubuntu 26.04 GNOME Wayland

Tested on Ubuntu 26.04.1 LTS, GNOME Wayland, with the VoxBar 1.2.x lineage (originally validated as Handy 0.9.8).

The fix is to use `ydotool` for typing and `VoxBar Keys` (`handy_keys`) for shortcuts.

_Note: Run commands one by one in terminal_

## 1. Check Wayland and uinput

```bash
echo "$XDG_SESSION_TYPE"
id
ls -l /dev/uinput
grep -R 'uinput' /etc/udev/rules.d /usr/lib/udev/rules.d 2>/dev/null
```

You should be using `Wayland`, be in the `input` group, and have `/dev/uinput` owned by `root:input` with mode `0660`. Ubuntu provides the required udev rule in `80-uinput.rules`.

If you are not in `input`:

```bash
sudo usermod -aG input "$USER"
```

Log out and back in after adding the group.

What happens if you skip this: with `VoxBar Keys` selected, startup now fails loudly instead of silently. The log shows `permission denied opening N device node(s) under /dev/input ... add your user to the 'input' group (sudo usermod -aG input $USER)`, the same message appears in the app, and VoxBar falls back to the Tauri shortcut backend for this launch and the next (the fallback is saved to settings). Re-grant the group and switch back to `VoxBar Keys` in Settings.

## 2. Install and test ydotool

```bash
sudo apt install ydotool
systemctl --user start ydotool
systemctl --user status ydotool --no-pager
ydotool type "HELLO FROM YDOTOOL"
```

The service should show `active (running)` and the test should type into the focused application.

## 3. Configure VoxBar

In Settings, set **Keyboard implementation** to **VoxBar Keys** and **Typing tool** to `ydotool`, or edit the settings file directly:

```bash
sed -i 's/"typing_tool": "auto"/"typing_tool": "ydotool"/' ~/.local/share/com.voxbar.app/settings_store.json
sed -i 's/"keyboard_implementation": "tauri"/"keyboard_implementation": "handy_keys"/' ~/.local/share/com.voxbar.app/settings_store.json
```

Restart VoxBar:

```bash
pkill voxbar
voxbar --start-hidden &
```

Check the VoxBar log for:

```text
handy-keys manager thread started
handy-keys shortcuts initialized
```

A missing `handy-keys shortcuts initialized` line plus a `Failed to create HotkeyManager` error means step 1 was not completed (group membership or `/dev/uinput` access); the message in the log names the exact fix.

If the `Tauri` keyboard backend is left active on a Wayland session, VoxBar warns once per launch (a notice in the app and one line in the log): global shortcuts may not reach native Wayland apps, because that backend is X11-only. That warning is expected until you switch to `VoxBar Keys`.

Then use your existing VoxBar shortcut and test dictation.

While a dictation is live, the Escape key cancels it (`VoxBar Keys` backend only; with the Tauri backend the tray menu's Cancel entry remains the only abort).

## 4. Hide the overlay

On GNOME, the overlay is a regular window that can take the focus, so nothing is pasted. Set **Overlay** to **None**.

VoxBar now warns about this itself: enabling the overlay on GNOME Wayland (where the layer-shell integration is unavailable) shows a one-time notice recommending **Overlay: None**. KDE and wlroots compositors support the layer-shell overlay and get no warning.

## 5. Install wl-clipboard

VoxBar uses `wl-copy` on Wayland when it is installed:

```bash
sudo apt install wl-clipboard
```

## 6. Non-QWERTY keyboard layouts

`ydotool` sends physical keys, so the built-in paste methods fail on layouts such as bépo or Dvorak: Ctrl+V presses the QWERTY `V` key, and Shift+Insert breaks while a modifier of your shortcut is still held.

Use an external script instead: it presses Ctrl and the key that types `v` on your layout, `KEY_U` (22) on bépo or `KEY_DOT` (52) on Dvorak (codes in `/usr/include/linux/input-event-codes.h`).

Create `~/.local/bin/voxbar-paste`, here for bépo:

```sh
#!/bin/sh
printf '%s' "$1" | wl-copy
sleep 0.1
ydotool key 29:1 22:1 22:0 29:0
```

```bash
chmod +x ~/.local/bin/voxbar-paste
```

Set **Paste Method** to **External Script** with this path.
