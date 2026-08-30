---
name: gnome-hig
description: >-
  Apply GNOME Human Interface Guidelines to Voxkey UI. Use when designing or
  reviewing settings screens, labels, icons, toasts, banners, navigation,
  density, light/dark style, or any GTK/libadwaita user-facing layout in
  voxkey-settings.
paths:
  - "voxkey-settings/**"
  - "gnome-shell-extension/**"
---

# GNOME HIG (Voxkey)

Follow the [GNOME Human Interface Guidelines](https://developer.gnome.org/hig/) for user-visible UI. Target recent GNOME with GTK 4 + libadwaita.

## Voxkey surfaces

- Primary settings app: `voxkey-settings/` (Adw `ApplicationWindow` + `NavigationSplitView`, preferences groups/rows)
- Shell Quick Settings tile/capsule: `gnome-shell-extension/` (Shell styling; still follow HIG writing and icon rules)

## Design rules

1. **One job per view.** Prefer clear pages/groups over dense dashboards.
2. **Prefer standard patterns.** Use Adw preferences groups, action/switch/combo/entry rows, banners, and toasts instead of custom chrome.
3. **Symbolic icons only.** Use `-symbolic` icon names; never emoji as UI icons.
4. **Writing.** Short labels, sentence case, actionable error text. Avoid implementation jargon in user-facing strings.
5. **Style.** Follow system light/dark via libadwaita. Test high-contrast. Do not invent a parallel theme.
6. **Feedback.** Transient success/errors → `AdwToast`. Persistent attention → `AdwBanner` or status rows. Destructive actions need confirmation.
7. **Adaptive.** Layouts should remain usable at narrow widths; avoid fixed wide forms.

## Settings copy checklist

- Titles name the setting, not the widget type
- Subtitles explain consequence, not implementation
- Empty/error states say what to do next
- Provider/engine names stay consistent with existing Settings pages

## References

- Full HIG index and pattern links: [references/resources.md](references/resources.md)
- Implementation patterns for this repo: load skill `libadwaita-rust`
