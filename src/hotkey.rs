//! Key combinations, as data. Nothing here touches an operating system.
//!
//! Keeping this layer portable is what lets the binding rules, the config
//! format and the settings UI all be developed and tested on macOS, leaving
//! `RegisterHotKey` as the only genuinely Windows-bound part of the feature.

use std::fmt;
use std::str::FromStr;

/// Virtual-key codes. These are the one place where the number genuinely is
/// the meaning: they are the values `RegisterHotKey` takes, defined by
/// Windows, and naming them adds nothing a comment cannot.
const VK_0: u16 = 0x30;
const VK_9: u16 = 0x39;
const VK_A: u16 = 0x41;
const VK_Z: u16 = 0x5A;
const VK_F1: u16 = 0x70;
/// F13 upward do not exist on an ordinary keyboard — see [`Key::is_typable`].
const VK_F13: u16 = 0x7C;
const VK_F24: u16 = 0x87;

const HIGHEST_FUNCTION_KEY: u16 = 24;

/// Keys with names of their own. Letters, digits and function keys are handled
/// by range instead: spelling out sixty variants would bury the dozen that are
/// actually special.
///
/// Where a key has more than one spelling the canonical one comes first, since
/// that is the one [`Key::name`] gives back.
const NAMED_KEYS: &[(&str, u16)] = &[
    ("Space", 0x20),
    ("Escape", 0x1B),
    ("Esc", 0x1B),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Return", 0x0D),
    ("Backspace", 0x08),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("Pause", 0x13),
];

/// The non-modifier key of a combination.
///
/// Held as its virtual-key code because that is what `RegisterHotKey`
/// ultimately needs, but never persisted or displayed that way: names round
/// trip through `FromStr` and `Display`, so the config file stays readable by
/// hand and an unknown name is rejected rather than silently bound to
/// whatever code it happened to resemble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(u16);

impl Key {
    /// Unused until the Win32 shell thread calls `RegisterHotKey`, which is
    /// the only thing that ever wants the raw code. The table it reads from is
    /// pinned by tests in the meantime.
    #[allow(dead_code)]
    pub fn virtual_key(self) -> u16 {
        self.0
    }

    /// Whether this key can be produced by someone typing normally.
    ///
    /// This is the test for whether a modifier is required. A global binding
    /// swallows its key in every application, so binding a bare `A` would mean
    /// the letter A could no longer be typed anywhere. F13 to F24 are absent
    /// from every ordinary keyboard and are exactly what a Stream Deck or a
    /// macro key emits, which makes an unmodified binding on them the point
    /// rather than a mistake.
    pub fn is_typable(self) -> bool {
        !(VK_F13..=VK_F24).contains(&self.0)
    }

    fn name(self) -> String {
        match self.0 {
            vk @ VK_A..=VK_Z => char::from(b'A' + (vk - VK_A) as u8).to_string(),
            vk @ VK_0..=VK_9 => char::from(b'0' + (vk - VK_0) as u8).to_string(),
            vk @ VK_F1..=VK_F24 => format!("F{}", vk - VK_F1 + 1),
            vk => NAMED_KEYS
                .iter()
                .find(|(_, code)| *code == vk)
                .map(|(name, _)| (*name).to_string())
                // Unreachable while `Key` is only constructible by parsing a
                // name. Kept as a label rather than a panic: this runs live,
                // and a strange-looking settings row beats a crash.
                .unwrap_or_else(|| format!("0x{vk:02X}")),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl FromStr for Key {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim();
        let upper = name.to_ascii_uppercase();

        if upper.len() == 1 {
            let byte = upper.as_bytes()[0];
            if byte.is_ascii_uppercase() {
                return Ok(Key(VK_A + u16::from(byte - b'A')));
            }
            if byte.is_ascii_digit() {
                return Ok(Key(VK_0 + u16::from(byte - b'0')));
            }
        }

        if let Some(number) = upper.strip_prefix('F') {
            if let Ok(n) = number.parse::<u16>() {
                if (1..=HIGHEST_FUNCTION_KEY).contains(&n) {
                    return Ok(Key(VK_F1 + n - 1));
                }
            }
        }

        NAMED_KEYS
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, vk)| Key(*vk))
            .ok_or_else(|| format!("\"{name}\" is not a key that can be bound"))
    }
}

/// A modifier-plus-key combination.
///
/// Deliberately a plain struct of flags rather than a bitfield: the settings
/// UI toggles these individually, and the Win32 layer is the only place that
/// cares what `MOD_CONTROL` is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: Key,
}

impl Hotkey {
    pub fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }

    /// Whether this combination is safe to register globally.
    ///
    /// Returns the reason on rejection so the settings UI can say why a
    /// capture was refused instead of appearing to ignore it.
    pub fn validate(&self) -> Result<(), String> {
        if self.has_modifier() || !self.key.is_typable() {
            return Ok(());
        }
        Err(format!(
            "{} needs at least one modifier. On its own it would be captured everywhere and could no longer be typed.",
            self.key
        ))
    }
}

impl fmt::Display for Hotkey {
    /// Modifiers in a fixed order so the same combination always reads the
    /// same way, whichever order the user happened to press them in.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (present, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.win, "Win"),
        ] {
            if present {
                write!(f, "{name}+")?;
            }
        }
        write!(f, "{}", self.key)
    }
}

impl FromStr for Hotkey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(format!("\"{s}\" is not a well-formed combination"));
        }

        // `split` never yields an empty vector, and the check above rules out
        // the single-empty-part case, so there is always a key to take.
        let (key_name, modifiers) = parts.split_last().expect("at least one part");

        let mut hotkey = Hotkey {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key: key_name.parse()?,
        };

        for modifier in modifiers {
            let slot = match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut hotkey.ctrl,
                "alt" => &mut hotkey.alt,
                "shift" => &mut hotkey.shift,
                "win" | "super" | "meta" => &mut hotkey.win,
                other => return Err(format!("\"{other}\" is not a modifier")),
            };
            if *slot {
                return Err(format!("\"{modifier}\" appears more than once"));
            }
            *slot = true;
        }

        hotkey.validate()?;
        Ok(hotkey)
    }
}

/// A combination to synthesize into another application, not to register.
///
/// Distinct from [`Hotkey`] because the two live under opposite rules. A
/// registered hotkey must not swallow a typable key, so `Hotkey` insists on a
/// modifier; a synthesized chord is whatever the target application's own
/// shortcut happens to be, and for a media player that is usually a bare `F`
/// or `Enter`. There is deliberately no Win modifier: synthesizing Win+key
/// would trigger operating-system shortcuts in the middle of someone's
/// desktop, and no player uses one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: Key,
}

impl fmt::Display for KeyChord {
    /// Modifiers in the same fixed order as [`Hotkey`], so chords and
    /// bindings read alike wherever they appear together.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (present, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")] {
            if present {
                write!(f, "{name}+")?;
            }
        }
        write!(f, "{}", self.key)
    }
}

impl FromStr for KeyChord {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(format!("\"{s}\" is not a well-formed combination"));
        }

        // `split` never yields an empty vector, and the check above rules out
        // the single-empty-part case, so there is always a key to take.
        let (key_name, modifiers) = parts.split_last().expect("at least one part");

        let mut chord = KeyChord {
            ctrl: false,
            alt: false,
            shift: false,
            key: key_name.parse()?,
        };

        for modifier in modifiers {
            let slot = match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut chord.ctrl,
                "alt" => &mut chord.alt,
                "shift" => &mut chord.shift,
                "win" | "super" | "meta" => {
                    return Err("Win cannot be synthesized: it would trigger system shortcuts".into());
                }
                other => return Err(format!("\"{other}\" is not a modifier")),
            };
            if *slot {
                return Err(format!("\"{modifier}\" appears more than once"));
            }
            *slot = true;
        }

        Ok(chord)
    }
}

/// What a hotkey press should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Send,
    Retrieve,
    /// Bring the bound media window back and press it to full screen. The
    /// manual fallback for a player the automatic watch missed — one that
    /// was already stowed before Send, or displaced by something other than
    /// WinSend.
    RestoreMedia,
}

impl Action {
    pub const ALL: [Action; 3] = [Action::Send, Action::Retrieve, Action::RestoreMedia];

    pub fn label(self) -> &'static str {
        match self {
            Action::Send => "Send",
            Action::Retrieve => "Retrieve",
            Action::RestoreMedia => "Restore Media",
        }
    }
}

/// The full set of bindings.
///
/// All are optional and all start unbound: registering a default would mean
/// quietly taking a combination from another application on first run, and
/// failing to do so would produce an error the user never asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hotkeys {
    pub send: Option<Hotkey>,
    pub retrieve: Option<Hotkey>,
    pub restore_media: Option<Hotkey>,
}

impl Hotkeys {
    pub fn binding(&self, action: Action) -> Option<Hotkey> {
        match action {
            Action::Send => self.send,
            Action::Retrieve => self.retrieve,
            Action::RestoreMedia => self.restore_media,
        }
    }

    /// Assign or clear a binding.
    ///
    /// Rejects a combination already used by the other action rather than
    /// letting one press mean two things, which would be resolved by whichever
    /// registration happened to win.
    pub fn set(&mut self, action: Action, hotkey: Option<Hotkey>) -> Result<(), String> {
        if let Some(hotkey) = hotkey {
            hotkey.validate()?;
            for other in Action::ALL.iter().filter(|a| **a != action) {
                if self.binding(*other) == Some(hotkey) {
                    return Err(format!("{hotkey} is already bound to {}", other.label()));
                }
            }
        }

        match action {
            Action::Send => self.send = hotkey,
            Action::Retrieve => self.retrieve = hotkey,
            Action::RestoreMedia => self.restore_media = hotkey,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Hotkey {
        s.parse().unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    #[test]
    fn combinations_round_trip_through_their_written_form() {
        for text in [
            "Ctrl+Alt+F9",
            "Ctrl+Shift+Win+A",
            "Alt+Space",
            "Ctrl+PageDown",
            "F13",
            "Shift+7",
        ] {
            assert_eq!(parse(text).to_string(), text);
        }
    }

    #[test]
    fn parsing_is_case_and_whitespace_insensitive() {
        assert_eq!(parse("ctrl + ALT + f9"), parse("Ctrl+Alt+F9"));
        assert_eq!(parse("CONTROL+escape"), parse("Ctrl+Esc"));
    }

    #[test]
    fn modifier_order_does_not_change_the_combination() {
        assert_eq!(parse("Alt+Ctrl+F9"), parse("Ctrl+Alt+F9"));
        // ...but it always reads back in one order, so the UI is stable.
        assert_eq!(parse("Alt+Ctrl+F9").to_string(), "Ctrl+Alt+F9");
    }

    #[test]
    fn unknown_names_are_rejected_rather_than_guessed() {
        for text in ["Ctrl+Alt+F99", "Ctrl+Nope", "Hyper+A", "Ctrl+", "+A", ""] {
            assert!(text.parse::<Hotkey>().is_err(), "{text:?} should not parse");
        }
    }

    #[test]
    fn a_repeated_modifier_is_a_mistake_worth_reporting() {
        assert!("Ctrl+Ctrl+A".parse::<Hotkey>().is_err());
    }

    #[test]
    fn an_ordinary_key_needs_a_modifier() {
        let plain = Hotkey { ctrl: false, alt: false, shift: false, win: false, key: "A".parse().unwrap() };
        assert!(plain.validate().is_err(), "a bare letter would stop being typable");
        assert!("A".parse::<Hotkey>().is_err());
    }

    /// The Stream Deck case: F13 upward cannot be typed, so requiring a
    /// modifier on them would rule out the hardware this is most likely to be
    /// driven from.
    #[test]
    fn function_keys_above_twelve_bind_on_their_own() {
        for text in ["F13", "F24"] {
            assert!(parse(text).validate().is_ok(), "{text} should bind alone");
        }
        assert!("F12".parse::<Hotkey>().is_err(), "F12 is on real keyboards");
    }

    #[test]
    fn the_same_combination_cannot_mean_two_things() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(parse("Ctrl+Alt+F9"))).unwrap();

        let clash = hotkeys.set(Action::Retrieve, Some(parse("Ctrl+Alt+F9")));
        assert!(clash.is_err(), "one press must not mean both actions");
        assert_eq!(hotkeys.retrieve, None, "a rejected binding must not be stored");
    }

    #[test]
    fn the_third_action_shares_the_no_duplicates_rule() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(parse("Ctrl+Alt+F9"))).unwrap();

        assert!(hotkeys.set(Action::RestoreMedia, Some(parse("Ctrl+Alt+F9"))).is_err());
        hotkeys.set(Action::RestoreMedia, Some(parse("Ctrl+Alt+F10"))).unwrap();
        assert_eq!(hotkeys.binding(Action::RestoreMedia), Some(parse("Ctrl+Alt+F10")));
    }

    #[test]
    fn rebinding_an_action_to_its_own_combination_is_allowed() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(parse("Ctrl+Alt+F9"))).unwrap();
        assert!(hotkeys.set(Action::Send, Some(parse("Ctrl+Alt+F9"))).is_ok());
    }

    #[test]
    fn bindings_can_be_cleared() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(parse("Ctrl+Alt+F9"))).unwrap();
        hotkeys.set(Action::Send, None).unwrap();
        assert_eq!(hotkeys.binding(Action::Send), None);
    }

    #[test]
    fn nothing_is_bound_by_default() {
        let hotkeys = Hotkeys::default();
        assert!(Action::ALL.iter().all(|a| hotkeys.binding(*a).is_none()));
    }

    fn parse_chord(s: &str) -> KeyChord {
        s.parse().unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    #[test]
    fn chords_round_trip_through_their_written_form() {
        for text in ["F", "Enter", "F11", "Alt+Enter", "Ctrl+Shift+F", "Space"] {
            assert_eq!(parse_chord(text).to_string(), text);
        }
    }

    /// The reason `KeyChord` exists at all: a bare letter is a perfectly good
    /// chord to send and a forbidden combination to register.
    #[test]
    fn a_bare_typable_key_is_a_valid_chord() {
        assert_eq!(parse_chord("F").key, "F".parse::<Key>().unwrap());
        assert!("F".parse::<Hotkey>().is_err(), "the same text must still be refused as a binding");
    }

    #[test]
    fn chord_parsing_is_case_and_whitespace_insensitive() {
        assert_eq!(parse_chord("alt + ENTER"), parse_chord("Alt+Enter"));
        assert_eq!(parse_chord("CONTROL+shift+f"), parse_chord("Ctrl+Shift+F"));
    }

    #[test]
    fn chords_reject_what_cannot_be_synthesized() {
        for text in ["Win+F", "Super+A", "meta+Enter", "Hyper+F", "Ctrl+Nope", "Ctrl+", "+F", "", "Alt+Alt+F"] {
            assert!(text.parse::<KeyChord>().is_err(), "{text:?} should not parse");
        }
    }

    #[test]
    fn virtual_keys_match_the_values_windows_defines() {
        assert_eq!("A".parse::<Key>().unwrap().virtual_key(), 0x41);
        assert_eq!("0".parse::<Key>().unwrap().virtual_key(), 0x30);
        assert_eq!("F1".parse::<Key>().unwrap().virtual_key(), 0x70);
        assert_eq!("F24".parse::<Key>().unwrap().virtual_key(), 0x87);
        assert_eq!("Escape".parse::<Key>().unwrap().virtual_key(), 0x1B);
    }

    #[test]
    fn alternative_spellings_normalise_to_the_canonical_name() {
        assert_eq!("Esc".parse::<Key>().unwrap().to_string(), "Escape");
        assert_eq!("Return".parse::<Key>().unwrap().to_string(), "Enter");
        assert_eq!(parse("Super+A").to_string(), "Win+A");
    }
}
