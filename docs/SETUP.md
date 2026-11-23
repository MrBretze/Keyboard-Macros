# Setup guide

Keyboard Macros needs one-time access to your keyboard and mouse:
- to **replay** macros, it simulates keyboard and mouse input;
- to **record** them, it reads your input devices.

If something is missing, the plugin's editor shows a red banner with a link to this page. After a fix, the About tab's **Run checks** button confirms that everything is in order.

- [Linux](#linux)
- [macOS](#macos)
- [Windows](#windows)

## Linux

The plugin needs:

| What | Used for | Error shown when it is missing |
|---|---|---|
| The `uinput` kernel module, and read/write access to `/dev/uinput` | Replaying macros | "the uinput kernel module is not loaded" or "no access to /dev/uinput" |
| Read access to `/dev/input/event*` | Recording macros | "no keyboard can be read in /dev/input" |

### 1. Load the uinput module

Most distributions load it automatically. If the plugin says the module is not loaded:

```bash
sudo modprobe uinput
```

To load it at every boot:

```bash
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
```

### 2. Get access to the input devices

Add your user to the `input` group:

```bash
sudo usermod -aG input $USER
```

Then **log out and log back in**, or restart the computer. Restarting OpenDeck alone is not enough, because group membership is only read when you log in.

**Fedora Atomic** (Silverblue, Kinoite, Bazzite…) and other image-based distributions keep the `input` group in `/usr/lib/group`, so `usermod` reports that the group does not exist. Copy it to `/etc/group` first, then run `usermod`:

```bash
grep -E '^input:' /usr/lib/group | sudo tee -a /etc/group
sudo usermod -aG input $USER
```

Check it after logging in again:

```bash
id -nG | grep -w input && echo "OK: you are in the input group"
```

> **Why the `input` group?** Members can read every keyboard and mouse on the computer, which is what recording needs. Only add accounts you trust.

#### Alternative: replay only, without the `input` group

If you only want to replay macros, not record them, you can give the logged-in user access to `/dev/uinput` alone, with a udev rule:

```bash
echo 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' \
  | sudo tee /etc/udev/rules.d/60-keyboard-macros-uinput.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then log out and back in. Recording stays unavailable without the `input` group, but you can still type macros into the editor or import them.

### 3. Restart OpenDeck

Then open the plugin's editor. The red banner should be gone. If it is still there, open the About tab and click **Run checks**: each failing line says what is still missing.

### OpenDeck installed as a Flatpak

No extra step compared with the steps above: the OpenDeck Flatpak already exposes the input devices, and the plugin uses your account's permissions.

## macOS

macOS asks the first time the plugin needs each permission. If a prompt was dismissed, or the editor shows the banner:

1. Open **System Settings → Privacy & Security**.
2. In **Accessibility**, turn on **OpenDeck**. This is needed to replay macros.
3. In **Input Monitoring**, turn on **OpenDeck**. This is needed to record macros.
4. Quit and restart OpenDeck.

If OpenDeck is not in the list, add it with the **+** button, from the Applications folder.

The macOS build is not signed by Apple. If macOS refuses to start it, open **System Settings → Privacy & Security** and allow it at the bottom of the page.

## Windows

Nothing needs to be set up. One limit: Windows does not let a program send keystrokes to a window that runs **as administrator**, unless OpenDeck itself runs as administrator.

## Still stuck?

[Open an issue](https://github.com/MrBretze/Keyboard-Macros/issues/new) and include the report from the About tab's **Copy report** button. Read the report before posting it: it can contain recorded keys if the verbose log was on.
