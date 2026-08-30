Name:           voxkey
Version:        0.6.2
Release:        1%{?dist}
Summary:        Wayland voice dictation daemon
License:        MIT
URL:            https://github.com/hy26v/voxkey

BuildRequires:  systemd-rpm-macros

Requires:       alsa-lib
Requires:       libxkbcommon
Requires:       gtk4
Requires:       libadwaita
%description
Press a key, speak, and your words appear as typed text in any Wayland
application. Uses XDG Desktop Portal interfaces for global shortcuts
and keyboard injection.

# sherpa-onnx shared libraries are bundled in /usr/lib64/voxkey/ and loaded
# via RPATH. Filter them out of RPM's auto-generated requires/provides so
# dnf doesn't expect a system package to supply them.
%global __requires_exclude ^lib(onnxruntime|sherpa-onnx-c-api)\\.so
%global __provides_exclude ^lib(onnxruntime|sherpa-onnx-c-api)\\.so

# Disable debug package generation and build steps — binaries are pre-built
%global debug_package %{nil}
%define _build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm

%install
rm -rf %{buildroot}

install -Dm755 %{_sourcedir}/voxkey %{buildroot}%{_bindir}/voxkey
install -Dm755 %{_sourcedir}/voxkey-settings %{buildroot}%{_bindir}/voxkey-settings
install -Dm755 %{_sourcedir}/keyboard-recovery %{buildroot}%{_bindir}/voxkey-keyboard-recovery

install -Dm755 %{_sourcedir}/libonnxruntime.so %{buildroot}%{_libdir}/voxkey/libonnxruntime.so
install -Dm755 %{_sourcedir}/libsherpa-onnx-c-api.so %{buildroot}%{_libdir}/voxkey/libsherpa-onnx-c-api.so
install -Dm644 %{_sourcedir}/ggml-silero-v6.2.0.bin \
    %{buildroot}%{_datadir}/voxkey/models/ggml-silero-v6.2.0.bin

install -Dm644 %{_sourcedir}/voxkey.service \
    %{buildroot}%{_userunitdir}/voxkey.service

install -Dm644 %{_sourcedir}/io.github.hy26v.Voxkey.desktop \
    %{buildroot}%{_datadir}/applications/io.github.hy26v.Voxkey.desktop

install -Dm644 %{_sourcedir}/io.github.hy26v.Voxkey.metainfo.xml \
    %{buildroot}%{_datadir}/metainfo/io.github.hy26v.Voxkey.metainfo.xml

install -Dm644 %{_sourcedir}/io.github.hy26v.Voxkey-512.png \
    %{buildroot}%{_datadir}/icons/hicolor/512x512/apps/io.github.hy26v.Voxkey.png
install -Dm644 %{_sourcedir}/io.github.hy26v.Voxkey-256.png \
    %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/io.github.hy26v.Voxkey.png
install -Dm644 %{_sourcedir}/io.github.hy26v.Voxkey-128.png \
    %{buildroot}%{_datadir}/icons/hicolor/128x128/apps/io.github.hy26v.Voxkey.png

install -Dm644 %{_sourcedir}/90-voxkey.preset \
    %{buildroot}%{_userpresetdir}/90-voxkey.preset

install -Dm644 %{_sourcedir}/gnome-shell-extension/metadata.json \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/metadata.json
install -Dm644 %{_sourcedir}/gnome-shell-extension/extension.js \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/extension.js
install -Dm644 %{_sourcedir}/gnome-shell-extension/constants.js \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/constants.js
install -Dm644 %{_sourcedir}/gnome-shell-extension/util.js \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/util.js
install -Dm644 %{_sourcedir}/gnome-shell-extension/capsule.js \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/capsule.js
install -Dm644 %{_sourcedir}/gnome-shell-extension/toggle.js \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/toggle.js
install -Dm644 %{_sourcedir}/gnome-shell-extension/stylesheet.css \
    %{buildroot}%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/stylesheet.css

%post
%systemd_user_post voxkey.service

%preun
%systemd_user_preun voxkey.service

%postun
%systemd_user_postun voxkey.service

%files
%{_bindir}/voxkey
%{_bindir}/voxkey-settings
%{_bindir}/voxkey-keyboard-recovery
%dir %{_libdir}/voxkey
%{_libdir}/voxkey/libonnxruntime.so
%{_libdir}/voxkey/libsherpa-onnx-c-api.so
%{_datadir}/voxkey/models/ggml-silero-v6.2.0.bin
%{_userunitdir}/voxkey.service
%{_userpresetdir}/90-voxkey.preset
%{_datadir}/gnome-shell/extensions/voxkey@hy26v.github.io/
%{_datadir}/applications/io.github.hy26v.Voxkey.desktop
%{_datadir}/metainfo/io.github.hy26v.Voxkey.metainfo.xml
%{_datadir}/icons/hicolor/512x512/apps/io.github.hy26v.Voxkey.png
%{_datadir}/icons/hicolor/256x256/apps/io.github.hy26v.Voxkey.png
%{_datadir}/icons/hicolor/128x128/apps/io.github.hy26v.Voxkey.png
