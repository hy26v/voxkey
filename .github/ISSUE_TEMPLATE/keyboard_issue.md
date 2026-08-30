---
name: Keyboard or input problem
about: Text not injected, wrong characters, stuck modifiers, or keyboard lockup
labels: bug, input
---

**What happened?**

A clear description. Examples: dictation produced no text, text appeared in
the wrong application, characters were dropped or substituted, a modifier key
(Shift/Ctrl/Alt/Super) behaves stuck after injection, or the whole session
stopped accepting keyboard input.

**When does it happen?**

- [ ] Every dictation
- [ ] Intermittently
- [ ] Only after screen lock/unlock
- [ ] Only after Voxkey restart or upgrade
- [ ] During rapid press/release of the shortcut

**Does the physical keyboard still work normally afterwards** (letters,
Shift on both sides, Ctrl, Alt, Super, copy/paste across two apps)?

**Environment (run each command and paste the output)**

```bash
gnome-shell --version
rpm -q xdg-desktop-portal xdg-desktop-portal-gnome mutter
busctl --user tree org.gnome.Mutter.RemoteDesktop
gsettings get org.gnome.desktop.input-sources sources
journalctl --user -u voxkey --since "-1h" --no-pager
```

- Fedora version:
- Wayland session? (must be yes):
- Input method framework in use (IBus with which engines?):
- Keyboard layout(s) and which is active:
- Voxkey version (`rpm -q voxkey`):
- Transcription provider and model:
- Did you run `scripts/keyboard-recovery`? Paste its full output if so.

**Shortcut in use**

The default is `Super+Alt+D`. If you chose another trigger, state it — some
chords collide with GNOME's own bindings.

**Anything else**

GNOME Shell log lines like "Received multiple virtual ... key presses"
are release blockers — include them if present.
