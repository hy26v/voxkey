# Fedora GNOME Boxes testing workflow

This runbook preserves the local Voxkey VM workflow for future development
sessions. The VM is managed by GNOME Boxes and exposed through libvirt's user
session; `virsh --connect qemu:///session` controls that same Boxes VM.

Use this VM for runtime, GUI, GNOME Shell, RPM, and task-specific testing when
the host must remain untouched. Do not install Voxkey, test dependencies, or
system packages on the host. Never put the VM password, API keys, or other
credentials in commands that will be committed, logs, screenshots, or this
document.

## Current local VM identifiers

Set these in each host shell rather than hard-coding them into commands:

```bash
vm_uri=qemu:///session
vm_name=fedora-unkno
vm_user=voxkeytest
vm_ssh_port=2222
```

Verify the identifiers before use because the VM may be renamed or recreated:

```bash
virsh --connect "$vm_uri" list --all
virsh --connect "$vm_uri" dominfo "$vm_name"
```

## Password-free agent access (required)

Never store the VM login password in this repository, in scripts, in docs, in
screenshots, or in commit messages. Agent sessions must not depend on knowing
or typing that password.

Access is restored and kept usable through three guest settings plus a host
SSH key:

1. **SSH key login** — host key `~/.ssh/voxkey_safety_vm_ed25519`, guest
   `~/.ssh/authorized_keys` for `voxkeytest`.
2. **GDM AutomaticLogin** — `/etc/gdm/custom.conf` logs in `voxkeytest` on
   boot so a reboot restores the Wayland session without a password prompt.
3. **Limited NOPASSWD sudo** — `/etc/sudoers.d/voxkey-test` allows only
   `/usr/bin/dnf`, `/usr/bin/rpm`, and `/usr/bin/systemctl` without a
   password (enough for `./scripts/local-install.sh`).
4. **No screen lock** — GNOME lock/idle lock disabled so SSH-only sessions
   are not blocked by a GDM lock screen.

Optional host SSH alias (create once on the developer machine; do not commit
private keys):

```sshconfig
Host voxkey-vm
    HostName 127.0.0.1
    Port 2222
    User voxkeytest
    IdentityFile ~/.ssh/voxkey_safety_vm_ed25519
```

After power-on, recreate port forwarding (below), then:

```bash
ssh -o BatchMode=yes voxkey-vm 'echo ok'
# or:
ssh -o BatchMode=yes -p "$vm_ssh_port" "$vm_user"@127.0.0.1 'echo ok'
```

### Prefer reboot over logout for unattended work

GDM AutomaticLogin runs on **boot**, not after a manual logout. Logging out
drops the session at the GDM password screen and blocks agent SSH GUI work.
To reload GNOME Shell / the Voxkey extension without interactive login:

```bash
virsh --connect "$vm_uri" reboot "$vm_name"
# wait, then re-add hostfwd and re-export session env vars over SSH
```

Use interactive logout/login only when a human is at the Boxes window.

### Repair access when SSH or autologin breaks

If BatchMode SSH fails, the desktop is stuck at GDM, or `sudo -n dnf`
prompts for a password, shut the VM down and repair the disk offline from
the host (needs Podman image `localhost/voxkey-guestfs:44` and the host
public key above):

```bash
./scripts/vm-repair-access.sh
```

The script shuts the domain off, injects authorized_keys, GDM autologin,
sudoers, and dconf lock-disable policy via guestfish, starts the VM, restores
port `2222` forwarding, and waits until Wayland plus passwordless `dnf` work.
It never reads, writes, or prints a login password.

Disk image path (Boxes default):

```bash
~/.local/share/gnome-boxes/images/fedora-unkno
```

## Start the VM and recreate SSH forwarding

Start the Boxes-managed domain if it is shut down:

```bash
virsh --connect "$vm_uri" start "$vm_name"
```

The QEMU user-network forwarding rule is ephemeral and disappears whenever the
VM powers off. Inspect it after each start:

```bash
virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --hmp 'info usernet'
```

If port `2222` is not already forwarded, add it:

```bash
virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --hmp \
  'hostfwd_add tcp:127.0.0.1:2222-:22'
```

Then connect without weakening host-key verification:

```bash
ssh -p "$vm_ssh_port" "$vm_user"@127.0.0.1
```

If SSH is unavailable, open the VM in GNOME Boxes and confirm that the guest is
booted, logged in, and running its SSH service. Fix guest-only problems from the
guest console; do not install or reconfigure anything on the host.

## Transfer the committed checkout

The reusable guest checkout is `~/voxkey`. Check it for work that must be
preserved before changing revisions:

```bash
ssh -p "$vm_ssh_port" "$vm_user"@127.0.0.1 \
  'cd ~/voxkey && git status --short && git rev-parse --short HEAD'
```

Commit the host change first, then transfer the exact commit with a Git bundle.
This works even when the commit has not been pushed to the remote repository.

On the host, from the Voxkey checkout:

```bash
vm_bundle="$(mktemp /tmp/voxkey-vm.XXXXXX.bundle)"
git bundle create "$vm_bundle" HEAD
scp -P "$vm_ssh_port" "$vm_bundle" \
  "$vm_user"@127.0.0.1:/tmp/voxkey.bundle
unlink "$vm_bundle"
```

In the guest:

```bash
cd ~/voxkey
git fetch /tmp/voxkey.bundle HEAD
git switch --detach FETCH_HEAD
git rev-parse --short HEAD
unlink /tmp/voxkey.bundle
```

`git switch` will refuse to overwrite conflicting tracked changes. Do not use
`git reset --hard`, `git clean`, or another destructive shortcut to bypass that
protection; inspect unexpected guest changes first.

## Build and test inside Fedora

The VM has limited memory relative to its virtual CPU count. Limit Cargo to four
parallel jobs:

```bash
cd ~/voxkey
CARGO_BUILD_JOBS=4 ./scripts/verify rust
```

Run isolated Python integration tests when the task needs them and their guest
dependencies are available:

```bash
cd ~/voxkey
CARGO_BUILD_JOBS=4 ./scripts/ci-integration
```

Any missing build or test dependencies belong in the VM, never on the host.
Report a missing dependency instead of silently weakening or skipping the
requested verification.

## Build and install the RPM in the guest

All guest system installation must remain RPM-owned. Build and install through
the repository helper. With the NOPASSWD sudoers rule in place, this needs no
password:

```bash
cd ~/voxkey
CARGO_BUILD_JOBS=4 ./scripts/local-install.sh
```

Never use `cargo install` and never copy Voxkey binaries into a system directory.
Confirm that the checkout and installed package contain the same Git revision:

```bash
git rev-parse --short=8 HEAD
rpm -q voxkey
```

The RPM release suffix contains the eight-character revision. The helper
restarts an already-running daemon, but a settings process left alive by “Keep
Running in Background” must still be fully terminated and reopened to load new
UI code.

One existing guest build cache has placed the two native runtime libraries in
`target/release/` while the packaging helper expected them in
`target/release/deps/`. Only if `local-install.sh` reports one of those exact
missing sources and the files already exist, repair the guest build cache with:

```bash
cp target/release/libonnxruntime.so target/release/deps/
cp target/release/libsherpa-onnx-c-api.so target/release/deps/
```

Then rerun `local-install.sh`. This copies only within the guest build tree; it
does not replace RPM-managed installation.

## Join the logged-in GNOME session over SSH

Run the following inside the guest SSH shell. These variables target the
logged-in user's D-Bus and Wayland session:

```bash
guest_uid="$(id -u)"
guest_runtime="/run/user/$guest_uid"
export XDG_RUNTIME_DIR="$guest_runtime"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$guest_runtime/bus"
export WAYLAND_DISPLAY=wayland-0
export GDK_BACKEND=wayland
```

Before launching a GUI, verify that the session endpoints exist:

```bash
test -S "$XDG_RUNTIME_DIR/bus"
test -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
```

If the Wayland socket has a different name, inspect
`/run/user/<guest uid>/wayland-*` and update `WAYLAND_DISPLAY` accordingly.

Fully close any stale settings process, then launch the installed desktop file:

```bash
pgrep -af voxkey-settings
pkill -TERM -x voxkey-settings
gtk-launch io.github.hy26v.Voxkey >/tmp/voxkey-settings.log 2>&1 &
```

Do not terminate a process until `pgrep` confirms the exact guest-only target.
Check the daemon and GUI logs with:

```bash
systemctl --user status voxkey.service
journalctl --user -u voxkey.service -n 200 --no-pager
sed -n '1,240p' /tmp/voxkey-settings.log
```

## D-Bus inspection and controlled state changes

Use the guest session bus for live diagnostics:

```bash
gdbus introspect --session \
  --dest io.github.hy26v.Voxkey.Daemon \
  --object-path /io/github/hy26v/Voxkey/Daemon

gdbus call --session \
  --dest org.freedesktop.DBus \
  --object-path /org/freedesktop/DBus \
  --method org.freedesktop.DBus.GetNameOwner \
  io.github.hy26v.Voxkey.Daemon
```

Properties are read through `org.freedesktop.DBus.Properties.Get`; there is no
`GetState` method. For example:

```bash
gdbus call --session \
  --dest io.github.hy26v.Voxkey.Daemon \
  --object-path /io/github/hy26v/Voxkey/Daemon \
  --method org.freedesktop.DBus.Properties.Get \
  io.github.hy26v.Voxkey.Daemon1 State
```

D-Bus setter calls modify the guest configuration. Record the original value
first and restore it after a focused test. Never send API keys through ad-hoc
D-Bus test commands. A reversible capture-format update check is:

```bash
gdbus call --session \
  --dest io.github.hy26v.Voxkey.Daemon \
  --object-path /io/github/hy26v/Voxkey/Daemon \
  --method io.github.hy26v.Voxkey.Daemon1.SetAudio 48000 2

gdbus call --session \
  --dest io.github.hy26v.Voxkey.Daemon \
  --object-path /io/github/hy26v/Voxkey/Daemon \
  --method io.github.hy26v.Voxkey.Daemon1.SetAudio 16000 1
```

## Screenshots and input from the host

Capture the complete VM display from the host:

```bash
virsh --connect "$vm_uri" screenshot "$vm_name" /tmp/voxkey-vm.png
```

Inspect the PNG with the session's image-viewing tool. Take a new screenshot
after every navigation or state transition rather than relying on remembered
coordinates.

QEMU absolute pointer coordinates range from `0` to `32767`. Convert a pixel
position from the current screenshot, move the pointer, and click with this host
shell helper:

```bash
vm_click() {
  local pixel_x="$1"
  local pixel_y="$2"
  local image_width="$3"
  local image_height="$4"
  local qemu_x=$((pixel_x * 32767 / image_width))
  local qemu_y=$((pixel_y * 32767 / image_height))

  virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --pretty \
    "{\"execute\":\"input-send-event\",\"arguments\":{\"events\":[{\"type\":\"abs\",\"data\":{\"axis\":\"x\",\"value\":$qemu_x}},{\"type\":\"abs\",\"data\":{\"axis\":\"y\",\"value\":$qemu_y}}]}}"
  virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --pretty \
    '{"execute":"input-send-event","arguments":{"events":[{"type":"btn","data":{"down":true,"button":"left"}}]}}'
  virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --pretty \
    '{"execute":"input-send-event","arguments":{"events":[{"type":"btn","data":{"down":false,"button":"left"}}]}}'
}
```

Pass the screenshot's actual dimensions on each call. The VM was previously
observed at `1920x1001`, but that is not a contract. Send keyboard input with
QEMU's human monitor commands:

```bash
virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --hmp 'sendkey esc'
virsh --connect "$vm_uri" qemu-monitor-command "$vm_name" --hmp \
  'sendkey alt-meta_l-d'
```

The second example sends `Alt+Super+D`, the default dictation shortcut. If the
display has blanked, `sendkey shift` can wake it. If the lock screen still
appears, run `./scripts/vm-repair-access.sh` rather than automating a password.

## GNOME Shell extension verification

The extension UUID is `voxkey@hy26v.github.io`. Inspect or enable it inside the
guest desktop session with:

```bash
gnome-extensions info voxkey@hy26v.github.io
gnome-extensions enable voxkey@hy26v.github.io
```

On Wayland, updated extension code requires a new GNOME Shell session;
restarting the settings application is not enough. For unattended agent work,
reboot the VM so GDM AutomaticLogin restores Wayland without a password
prompt, then re-export the desktop-session variables before testing Quick
Settings. Interactive logout/login is fine only when a human can unlock GDM.

A leftover `~/.local/share/gnome-shell/extensions/voxkey@hy26v.github.io`
overrides the RPM under `/usr/share/...`. After installing the package,
confirm `gnome-extensions info voxkey@hy26v.github.io` reports the system
`Path:` and remove any stale user copy before rebooting.

Use the top-right Quick Settings menu to inspect the Voxkey tile and its menu.
Exercise start, finish, cancel, transcript, error, engine, microphone, shortcut,
and settings-deep-link states relevant to the change. Capture screenshots of
the actual states under review.

## End-of-run checklist

Before reporting completion:

- Confirm the guest checkout revision and RPM suffix match.
- Run the relevant verification commands in Fedora and record their results.
- Reopen the installed settings process after every UI RPM update.
- After every extension update, reboot (preferred for agents) or interactively
  log out and back in, then rejoin the session over SSH.
- Restore configuration values changed solely for testing.
- Check `git status --short` on the host and preserve unrelated changes.
- State explicitly that nothing was installed on the host.

Leave the VM running unless the user asks for shutdown. If shutdown is wanted,
use GNOME's normal shutdown or `virsh shutdown`; never use `virsh destroy` for
routine cleanup.
