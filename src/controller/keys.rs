//! Shortcuts as data: (key, modifiers) -> command. Views translate their key events into
//! [`Chord`] and look the command up; no view decides what a key does.

/// The keys legcon binds; views map their native key codes onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Digit(u8),
    F(u8),
    Tab,
    Enter,
    Backspace,
    Delete,
    Up,
    Down,
}

/// Modifiers; `primary` is Ctrl on Windows and Linux, Cmd on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub primary: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub key: Key,
    pub mods: Mods,
}

/// What a shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Swap,
    ToggleLocalTree,
    ToggleRemoteTree,
    LocalHome,
    LeaveHome,
    Refresh,
    Up,
    CloseTab,
    NextTab,
    PrevTab,
    ShowMachines,
    /// Copy the selection to the other pane.
    Copy,
    Rename,
    Delete,
    NewFolder,
}

const fn chord(key: Key, primary: bool, shift: bool) -> Chord {
    Chord {
        key,
        mods: Mods {
            primary,
            shift,
            alt: false,
        },
    }
}

/// The default bindings (docs/PLAN.md §1.1).
pub const DEFAULTS: &[(Chord, Command)] = &[
    (chord(Key::Char('S'), true, true), Command::Swap),
    (chord(Key::Digit(1), true, true), Command::ToggleLocalTree),
    (chord(Key::Digit(2), true, true), Command::ToggleRemoteTree),
    (chord(Key::Char('H'), true, true), Command::LocalHome),
    (chord(Key::Char('L'), true, true), Command::LeaveHome),
    (chord(Key::F(5), false, false), Command::Refresh),
    (chord(Key::Backspace, false, false), Command::Up),
    (chord(Key::Char('W'), true, false), Command::CloseTab),
    (chord(Key::Tab, true, false), Command::NextTab),
    (chord(Key::Tab, true, true), Command::PrevTab),
    (chord(Key::Digit(0), true, false), Command::ShowMachines),
    (chord(Key::F(6), false, false), Command::Copy),
    (chord(Key::F(2), false, false), Command::Rename),
    (chord(Key::Delete, false, false), Command::Delete),
    (chord(Key::Char('N'), true, false), Command::NewFolder),
];

/// The command bound to `c`, if any. Letters match whatever their case.
pub fn command_for(c: Chord) -> Option<Command> {
    let c = match c.key {
        Key::Char(ch) => Chord {
            key: Key::Char(ch.to_ascii_uppercase()),
            ..c
        },
        _ => c,
    };
    DEFAULTS.iter().find(|(k, _)| *k == c).map(|(_, cmd)| *cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_is_primary_shift_s_and_unique() {
        let c = Chord {
            key: Key::Char('s'),
            mods: Mods {
                primary: true,
                shift: true,
                alt: false,
            },
        };
        assert_eq!(command_for(c), Some(Command::Swap));
        for (i, (a, _)) in DEFAULTS.iter().enumerate() {
            assert!(
                DEFAULTS[i + 1..].iter().all(|(b, _)| a != b),
                "{a:?} bound twice"
            );
        }
    }
}
