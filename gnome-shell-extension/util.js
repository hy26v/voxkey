// ABOUTME: Pure text helpers for Quick Settings labels and capsule previews.
// ABOUTME: Truncation, menu previews, and human-readable shortcut formatting.

export function truncateText(text, maximum, keepTail = false) {
    const characters = [...(text ?? '')];
    if (characters.length <= maximum)
        return characters.join('');
    const retained = keepTail
        ? characters.slice(-(maximum - 1))
        : characters.slice(0, maximum - 1);
    return keepTail ? `…${retained.join('')}` : `${retained.join('')}…`;
}

export function menuPreview(text, maximum) {
    return truncateText((text ?? '').replace(/\s+/g, ' ').trim(), maximum);
}

export function readableShortcut(description) {
    const trimmed = (description ?? '').trim();
    const hasPressPrefix = trimmed.startsWith('Press ');
    let remainder = hasPressPrefix ? trimmed.slice('Press '.length) : trimmed;
    const parts = [];

    while (remainder.startsWith('<')) {
        const end = remainder.indexOf('>');
        if (end < 0)
            return trimmed;
        const modifier = remainder.slice(1, end);
        const modifierNames = {
            control: 'Ctrl',
            primary: 'Ctrl',
            ctrl: 'Ctrl',
            alt: 'Alt',
            mod1: 'Alt',
            shift: 'Shift',
            super: 'Super',
            mod4: 'Super',
            meta: 'Meta',
            hyper: 'Hyper',
        };
        parts.push(modifierNames[modifier.toLowerCase()] ?? modifier);
        remainder = remainder.slice(end + 1);
    }

    if (parts.length === 0 || !remainder)
        return trimmed;

    const keyNames = {
        space: 'Space',
        return: 'Enter',
        escape: 'Esc',
    };
    const key = keyNames[remainder.toLowerCase()] ??
        (remainder.length === 1 ? remainder.toUpperCase() : remainder);
    parts.push(key);
    return `${hasPressPrefix ? 'Press ' : ''}${parts.join(' + ')}`;
}
