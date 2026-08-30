# 2026-07-18 keyboard lockup after Voxkey restart

## Status

The incident ended with a reboot after no reliable in-session recovery for
Shift was found. Before the reboot, ordinary typing was partially restored by
disabling Voxkey and selecting a valid fallback XKB engine, but Shift remained
broken. After the reboot, Voxkey was verified as `masked` and `inactive`.

The emergency input-source change survived the reboot: the machine now has US
first and EurKEY second, with US selected. Before the incident it had only
EurKEY (`[('xkb', 'eu')]`). This is recovery residue, not the intended final
configuration.

Do not start Voxkey again until the cause is understood and recovery tests
have passed in an isolated graphical session or disposable VM.

## User impact

- The graphical session stopped accepting normal keyboard input.
- IBus appeared to consume key events; Return could not be sent.
- Recovery had to be performed from a remote TTY/SSH shell.
- After partial recovery, ordinary keys worked but Shift did not.

## Timeline and evidence

- `13:31:34`: `voxkey.service` started and restored its persisted
  RemoteDesktop token.
- `13:33:34` and `13:33:37`: Voxkey injected two transcripts through the
  RemoteDesktop portal.
- At both injection times GNOME Shell logged duplicate virtual key events,
  including `Received multiple virtual ... key presses/releases (ignoring)`.
- `13:34:18`: Voxkey received a D-Bus quit request and stopped.
- Stopping the process was insufficient because the installed D-Bus service
  can activate it again. The README documents that the user service must also
  be disabled and masked.
- Restarting IBus exposed a second failure: GNOME Shell requested the configured
  EurKEY engine `xkb:eu::swe`, but IBus reported `Cannot find engine
  xkb:eu::swe`. At that point IBus listed the Voxkey engine but no matching
  EurKEY engine.
- Selecting `xkb:us::eng` restored ordinary typing. Shift remained broken.
- A temporary session created directly through Mutter's RemoteDesktop API
  successfully sent valid press/release pairs for both Shift keys and the
  other common modifiers. No successful Shift recovery was confirmed after
  that reset; the machine was ultimately rebooted.
- The reboot ended the live incident. It did not undo the emergency GNOME
  input-source change, but the service mask persisted as intended.

## Confirmed facts versus hypotheses

Confirmed:

- Voxkey started with a persisted RemoteDesktop restore token immediately
  before the failure.
- GNOME Shell logged duplicate virtual key press/release events during both
  transcript injections.
- Stopping Voxkey alone did not constitute a complete disable because D-Bus
  activation remained available.
- Restarting IBus made recovery worse by leaving GNOME Shell unable to find
  its configured `xkb:eu::swe` engine.
- Selecting `xkb:us::eng` restored only partial keyboard function.
- Shift was not restored during the running session; a reboot was used.

Not established:

- Whether the initial total lockout was caused by IBus, a portal session,
  Mutter's virtual-keyboard state, or an interaction among them.
- Whether the duplicate virtual events caused the dead Shift state or were
  only a symptom.
- Whether a graphical logout without a reboot would have restored Shift.
- Whether the optional Voxkey IBus component caused the missing EurKEY engine.
  Its presence in the registry is suspicious but not proof of causation.
- Whether closing the portals helped. It was done alongside other changes, so
  the effect was not isolated.

## Post-incident GNOME 50.3 source audit

The following findings come from the exact Fedora source packages matching the
installed stack (`mutter-50.3-2.fc44`, `xdg-desktop-portal-1.22.1-1.fc44`, and
`xdg-desktop-portal-gnome-50.0-1.fc44`). They explain why several initial
assumptions were invalid, but they do not by themselves prove the incident's
root cause.

- A successful reply from the public portal's `NotifyKeyboardKeysym` method is
  not an acknowledgement from Mutter's input thread. `xdg-desktop-portal`
  forwards the call asynchronously and replies immediately;
  `xdg-desktop-portal-gnome` does the same when forwarding to Mutter. Mutter
  then schedules the actual key work as a high-priority idle source on its
  input context. Waiting for each public D-Bus reply therefore does not create
  the processing barrier previously claimed.
- Mutter resolves a keysym to a keycode and level. For shifted characters it
  synthesizes Shift around the character. The virtual-device pressed-key table
  is updated for the resolved character keycode, while the synthesized level
  modifier is sent directly to the seat. Virtual-device destruction releases
  keys recorded in the virtual-device table. This creates a credible stuck-
  modifier failure mechanism if the synthesized sequence becomes unbalanced,
  but the incident evidence does not establish that this is what happened.
- Mutter's EIS path accepts explicit keycodes and records every pressed key in
  a per-client key-state bitmap. When an EIS client disconnects, Mutter walks
  that bitmap and releases every recorded key before removing the client.
- The EIS protocol supplies the compositor's current XKB keymap and provides a
  synchronization callback. The callback is a real protocol barrier for prior
  key requests and their directly resulting modifier-state events, unlike the
  public portal notify-method reply. The libei specification explicitly does
  not extend that guarantee to indirectly triggered compositor actions, such
  as a shortcut implemented outside the keymap; this remains a live-validation
  concern rather than something the offline tests can prove.
- In Mutter 50.3, the modifier event sent to each EIS keyboard is populated
  from the default seat's `ClutterKeymap`, and Mutter subscribes the client to
  that keymap's state-change signal. This supports using the event to reject
  injection while a physical modifier is down; it is not merely the virtual
  keyboard's private state. It is still only a synchronized snapshot. A
  physical key can change immediately after the callback, so the offline audit
  cannot claim an atomic "no physical modifier" guarantee.
- Voxkey's old default shortcut, `Super+Space`, is GNOME's documented default
  for switching to the next input source (`Shift+Super+Space` selects the
  previous source). Even though the GlobalShortcuts portal can present a
  configuration UI, requesting GNOME's own input-source chord was an avoidable
  collision in exactly the subsystem affected by this incident. The default
  is now `Super+Alt+D`; old default configs are migrated in memory, and both
  GNOME input-source chords are rejected by the settings API. The daemon also
  inspects the portal's returned binding and closes the shortcut session if a
  previously persisted portal assignment still resolves to either chord.
- Removing the IBus component from packaging was not enough while the main
  daemon still exposed an `ibus_engine_active` control bridge. A stale engine
  from an older installation could still change the daemon's injection path.
  That property, method, owner watcher, and every runtime IBus branch have now
  been removed from the production daemon. The obsolete prototype crate and
  component descriptor have also been deleted from the repository.

Based on those findings, the in-development injector no longer calls
`NotifyKeyboardKeysym`. The portal specification permits `ConnectToEIS` only
once per RemoteDesktop session, so an earlier short-lived-per-transcript design
was invalid and was removed before live use. The current design creates that
one EIS connection lazily for the first batch, keeps it neutral while idle,
maps a whole batch using the compositor-
provided keymap before sending its first key, sends explicit keycodes for both
characters and safe level modifiers, and waits for an EIS synchronization
callback after every character. A mapping, protocol, timeout, or
synchronization error disconnects EIS, closes the RemoteDesktop session, and
terminates the daemon without retrying partial text. This design is source-
audited and covered by offline tests; it has deliberately not been run in the
affected graphical session and must not be described as proven safe before
isolated live validation.

Primary references used for this audit:

- [XDG RemoteDesktop portal specification](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
- [libei sender API](https://libinput.pages.freedesktop.org/libei/api/group__libei-sender.html)
- [libei device API](https://libinput.pages.freedesktop.org/libei/interfaces/ei_device/index.html)
- [libxkbcommon keyboard-state API](https://xkbcommon.org/doc/current/group__state.html)
- [Mutter 50 source releases](https://download.gnome.org/sources/mutter/50/)
- [GNOME input-source shortcut documentation](https://help.gnome.org/users/gnome-help/stable/keyboard-layouts.html)

## Recovery actions performed

```sh
systemctl --user disable --now voxkey.service
systemctl --user mask voxkey.service
pkill -TERM -x voxkey
pkill -TERM -f '^/usr/libexec/voxkey-ibus-engine([[:space:]]|$)'

systemctl --user restart xdg-desktop-portal-gnome.service
systemctl --user restart xdg-desktop-portal.service
systemctl --user restart org.freedesktop.IBus.session.GNOME.service

gsettings set org.gnome.desktop.input-sources sources \
  "[('xkb', 'us'), ('xkb', 'eu')]"
gsettings set org.gnome.desktop.input-sources current 0
```

The active IBus address was then read from the graphical session's file under
`~/.config/ibus/bus/`, and `xkb:us::eng` was selected explicitly. Voxkey was
verified as `masked` and `inactive`.

## Post-reboot cross-application clipboard failure and recovery

Later in the post-reboot session, copy and paste appeared to work only within
the application that owned the text. GNOME Text Editor could copy and paste
back into itself, and a standalone GNOME Terminal could copy and paste back
into itself, but newly copied text did not appear to cross between those two
applications. Chrome-to-Terminal exhibited the same symptom. The user reported
that this behavior had not occurred before the Voxkey/IBus work earlier that
day.

Both test applications were confirmed to be native Wayland clients connected
to the same `wayland-0` socket. A metadata-only selection trace also showed
that explicit Copy in Text Editor changed the regular `CLIPBOARD` selection,
while selecting text in Terminal changed the independent `PRIMARY` selection.
That distinction can produce the same visible symptom when Terminal mouse
selection/middle-click is mixed with application Copy/Paste commands. However,
the user reported a real regression, and the investigation did not isolate
whether the two-selection behavior fully explained it.

The following persistent Voxkey state was found even though the daemon was
already masked and inactive:

- eight saved RemoteDesktop grants across the historical application ID
  `com.github.voxkey` and current ID `io.github.hy26v.Voxkey`;
- saved GNOME global-shortcut registrations for both application IDs;
- `~/.config/voxkey/restore_token`;
- an RPM-installed IBus descriptor and engine, which made `voxkey` appear in
  `ibus list-engine` even though it was not selected or running;
- an RPM-installed D-Bus activation file. The systemd user-unit mask blocked
  this activation path during recovery.

The user-session state was backed up and cleaned with the following procedure.
The backup directory name is intentionally unique so a second recovery cannot
overwrite the first one. The backup from this incident is
`~/.local/state/voxkey-recovery-20260718-3hD1bF`.

```sh
mkdir -p "$HOME/.local/state"
recovery_dir=$(mktemp -d "$HOME/.local/state/voxkey-recovery-XXXXXXXX")
cp --preserve=all "$HOME/.config/dconf/user" \
  "$recovery_dir/dconf-user.before"
cp --preserve=all "$HOME/.local/share/flatpak/db/remote-desktop" \
  "$recovery_dir/remote-desktop-permissions.before"

if test -f "$HOME/.config/voxkey/restore_token"; then
  mv "$HOME/.config/voxkey/restore_token" \
    "$recovery_dir/restore_token.removed"
fi

flatpak permission-reset io.github.hy26v.Voxkey
flatpak permission-reset com.github.voxkey

dconf reset -f \
  /org/gnome/settings-daemon/global-shortcuts/io.github.hy26v.Voxkey/
dconf reset -f \
  /org/gnome/settings-daemon/global-shortcuts/com.github.voxkey/
```

The two Voxkey IDs were also removed from
`org.gnome.settings-daemon.global-shortcuts applications`. Do not copy the
literal application list from this machine: first read it and preserve every
unrelated ID when writing the filtered list back.

```sh
gsettings get org.gnome.settings-daemon.global-shortcuts applications
# Then use `gsettings set` with the same list minus only:
#   'com.github.voxkey'
#   'io.github.hy26v.Voxkey'
```

After cleanup, these checks showed no active Voxkey user-session state:

```sh
flatpak permission-list remote-desktop | grep -i voxkey
systemctl --user is-enabled voxkey.service  # expected: masked
systemctl --user is-active voxkey.service   # expected: inactive
pgrep -a -f '^(/usr/bin/voxkey|/usr/libexec/voxkey-ibus-engine)([[:space:]]|$)'
ibus engine                                 # expected: a normal XKB engine
```

The installed RPM still exposed the inactive Voxkey engine in
`ibus list-engine`; removing or replacing that RPM is required to remove the
system IBus descriptor and D-Bus activation file. That system-level removal
was **not** performed before clipboard functionality returned, so it must not
be credited as part of this recovery.

The verification deliberately used application menu actions to avoid
confusing `PRIMARY` with `CLIPBOARD`:

1. GNOME Text Editor **Copy** -> GNOME Terminal **Paste**.
2. GNOME Terminal **Copy** -> GNOME Text Editor **Paste**.

Both directions worked after the combined cleanup. Because the permission
grants, shortcut records, and restore token were removed together, the action
that restored cross-application clipboard behavior is not known. Future
investigations should preserve evidence and change one item at a time when the
machine is not at risk of input lockout. Do not restart IBus merely to repair
clipboard behavior; the earlier IBus restarts worsened this incident.

## Important failed recovery attempts

- Restarting IBus alone did not restore input. It invalidated the graphical
  session's IBus connection and left no valid EurKEY engine registered.
- `ibus write-cache --system` could not update `/var/cache/ibus/bus/registry`
  without root privileges and did not repair the session.
- `wtype` could not send modifier-release events because GNOME reported that
  the compositor does not support the virtual-keyboard protocol.
- Xwayland `xdotool` recovery was unavailable because the GNOME Wayland
  session did not expose a `DISPLAY` in the Shell process environment.
- GNOME Shell's D-Bus `Eval` method was disabled in the running session, so it
  could not be used to inspect Mutter's modifier mask.
- A bare modifier-release event sent through a temporary Mutter RemoteDesktop
  session was correctly rejected as `Invalid key event` because that temporary
  device had not pressed the key.
- Valid modifier press/release pairs through a temporary Mutter session
  completed, but did not produce a confirmed Shift recovery.
- Raw physical input events could not be inspected from the remote account:
  `/dev/input/event*` was unreadable and passwordless sudo was unavailable.

## What went wrong in the handling of this incident

- Voxkey had previously been described as safe without evidence from a live,
  end-to-end GNOME Wayland test covering injection, shutdown, and continued
  physical keyboard operation. That safety claim was unjustified.
- The first recovery response stopped and restarted processes before reading
  the repository's own warning that `stop` is insufficient because D-Bus can
  reactivate the daemon.
- IBus was restarted without first preserving its live address, registered
  engines, and GNOME input-source state. That introduced a second failure and
  complicated diagnosis.
- A fallback US input source was added to recover basic typing, but the
  persistent configuration change was not immediately treated as cleanup
  debt.
- Suggested logout/login recovery was presented too readily despite the high
  cost of ending the user's graphical session and the lack of proof that it
  would work.
- Remote-shell availability reduced the chance of permanent lockout, but it
  did not make testing on the user's primary graphical session safe.

## Lessons and required changes

1. Never call an input-injection build "safe" based on unit tests, compilation,
   code inspection, or daemon health. Safety requires a live end-to-end test
   that verifies the physical keyboard after every injection and shutdown.
2. Do not run the next test on the primary graphical session. Use a disposable
   VM or isolated GNOME login with working remote access and no valuable open
   applications.
3. Treat the daemon, its D-Bus activator, the portal sessions, and Mutter's
   virtual input device as separate lifecycle layers. Stopping one layer does
   not prove the others are gone.
4. Delete abandoned input-method integrations instead of retaining an
   uninstalled implementation that can be mistaken for a supported path. The
   obsolete IBus prototype and its component descriptor have been removed.
5. Build and test a recovery command before another live trial. It must disable
   and mask all activation paths, close sessions, preserve the current input
   source, report remaining virtual devices, and verify modifier behavior.
6. Never restart IBus blindly during keyboard recovery. First save the GNOME
   input-source settings and current IBus address/engine, and understand how
   the compositor will re-register its XKB engines.
7. Add tests for uppercase text, every modifier, repeated rapid injections,
   shutdown during injection, portal failure during press/release, restore-token
   reconnection, D-Bus reactivation, and post-injection physical typing.
8. Treat GNOME Shell messages about duplicate virtual key events as a release
   blocker, even if text injection appears to succeed.
9. Ensure every synthetic press has a release on success, cancellation, error,
   timeout, and shutdown paths. Add cleanup that is explicit and observable.
10. Preserve evidence and configuration before recovery mutations. Record each
    command independently so an apparent improvement can be attributed to one
    action.
11. Do not propose logout or reboot as routine recovery. Exhaust validated
    in-session methods first, then state clearly when no tested live fix exists
    and let the user decide how to protect unsaved work.
12. Do not equate a portal method reply with compositor input completion. Read
    the frontend, backend, compositor, and input-thread implementations before
    assigning acknowledgement or ordering semantics.

## Safety requirements before another run

1. Keep `voxkey.service` masked on the affected machine.
2. Do not rely on `systemctl --user stop`; D-Bus activation must be blocked.
3. Reproduce only in an isolated graphical session or disposable VM with
   remote-shell access already verified.
4. Add an integration/recovery test that proves all synthetic key presses have
   matching releases, including error and shutdown paths.
5. Test shutdown immediately after injection and confirm modifiers still work.
6. Investigate why GNOME logged duplicate virtual key events and why the
   EurKEY IBus engine disappeared after restart.
7. Document a tested, non-destructive method to reset stuck modifier state on
   GNOME Wayland. No such method was established in this incident; a reboot was
   ultimately used.
8. Restore the user's intended EurKEY-only input-source configuration only
   after confirming the post-reboot keyboard is healthy and with the user's
   agreement.
