// ABOUTME: GNOME Shell extension entry point for Voxkey Quick Settings.
// ABOUTME: Creates and destroys the system indicator on enable/disable.

import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import { VoxkeyIndicator } from './toggle.js';

export default class VoxkeyExtension extends Extension {
    enable() {
        this._indicator = new VoxkeyIndicator();
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
    }

    disable() {
        if (!this._indicator)
            return;
        // VoxkeyIndicator.destroy() tears down quickSettingsItems and the capsule.
        this._indicator.destroy();
        this._indicator = null;
    }
}
