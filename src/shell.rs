//! The seam between WinSend and the desktop shell: global hotkeys now, the
//! notification area next.
//!
//! Separate from `Platform` because the dependency runs the other way. A
//! platform call is a question WinSend asks the OS and gets an answer to on the
//! spot. The shell instead delivers events on the OS's schedule, from a thread
//! of its own, and that difference is what shapes everything below.

use crate::hotkey::{Action, Hotkeys};

/// Wakes the UI thread. Called from the shell's own thread as soon as an event
/// is queued.
///
/// A closure rather than an `egui::Context` so this layer knows nothing about
/// the UI toolkit. It is not optional: an idle eframe window never calls
/// `update`, so without a wake a hotkey press would do nothing until the user
/// moved the mouse over the window they pressed the hotkey to avoid touching.
pub type Waker = Box<dyn Fn() + Send + Sync>;

/// Something the user asked for from outside the main window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// A bound combination was pressed.
    Trigger(Action),
    /// The outcome of the most recent [`Shell::apply_hotkeys`].
    HotkeysApplied(HotkeyReport),
}

/// Which bindings the OS refused, and why.
///
/// `RegisterHotKey` fails when another application already owns a combination.
/// That has to reach the user: a hotkey that silently does nothing, discovered
/// mid-broadcast, is the worst version of this feature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotkeyReport {
    pub rejected: Vec<(Action, String)>,
}

impl HotkeyReport {
    /// Read by the settings screen, which marks the offending row rather than
    /// leaving the reason only in the status bar.
    pub fn reason(&self, action: Action) -> Option<&str> {
        self.rejected
            .iter()
            .find(|(rejected, _)| *rejected == action)
            .map(|(_, why)| why.as_str())
    }

    /// One line covering every refusal, for the status bar.
    pub fn summary(&self) -> Option<String> {
        if self.rejected.is_empty() {
            return None;
        }
        Some(
            self.rejected
                .iter()
                .map(|(action, why)| format!("{} hotkey unavailable: {why}", action.label()))
                .collect::<Vec<_>>()
                .join("  "),
        )
    }
}

pub trait Shell {
    /// Replace every registered binding with this set.
    ///
    /// Deliberately returns nothing. Registration happens on the shell's own
    /// thread, so a synchronous result would mean blocking the UI thread on a
    /// channel round trip, and a thread that had died would hang the app
    /// outright. The outcome arrives as [`ShellEvent::HotkeysApplied`] a frame
    /// later instead.
    ///
    /// Passing `Hotkeys::default()` unregisters everything, which is what the
    /// settings screen does while capturing a combination: a registered hotkey
    /// is swallowed by the OS and never reaches the UI, so rebinding a key to
    /// itself would otherwise appear to do nothing at all.
    fn apply_hotkeys(&self, hotkeys: Hotkeys);

    /// Everything that has happened since the last call.
    fn poll(&self) -> Vec<ShellEvent>;

    /// Escape hatch for the mock-only debug controls, mirroring
    /// [`crate::platform::Platform::as_mock`]. Absent from Windows builds, so
    /// it cannot leak into the shipped binary.
    #[cfg(not(windows))]
    fn as_mock(&self) -> Option<&crate::mock::MockShell> {
        None
    }
}

/// The real shell on Windows, the fake one everywhere else.
///
/// Mirrors `select_platform`, including honouring `WINSEND_MOCK=1`, so the UI
/// can be worked on without taking real hotkeys off the machine running it.
pub fn create(waker: Waker) -> Box<dyn Shell> {
    // The Win32 implementation lands with the message-only window; until then
    // every target gets the mock, which keeps the app runnable throughout.
    Box::new(crate::mock::MockShell::new(waker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;
    use crate::mock::MockShell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn hotkey(text: &str) -> Hotkey {
        text.parse().unwrap()
    }

    fn shell() -> (MockShell, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let shell = MockShell::new(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        (shell, wakes)
    }

    #[test]
    fn applying_bindings_reports_back() {
        let (shell, _) = shell();
        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(hotkey("Ctrl+Alt+F9"))).unwrap();

        shell.apply_hotkeys(hotkeys);

        assert_eq!(
            shell.poll(),
            vec![ShellEvent::HotkeysApplied(HotkeyReport::default())]
        );
    }

    #[test]
    fn a_combination_another_application_owns_comes_back_rejected() {
        let (shell, _) = shell();
        shell.set_unavailable(vec![hotkey("Ctrl+Alt+F9")]);

        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(hotkey("Ctrl+Alt+F9"))).unwrap();
        hotkeys.set(Action::Retrieve, Some(hotkey("Ctrl+Alt+F10"))).unwrap();
        shell.apply_hotkeys(hotkeys);

        let [ShellEvent::HotkeysApplied(report)] = &shell.poll()[..] else {
            panic!("expected exactly one report");
        };
        assert!(report.reason(Action::Send).is_some());
        assert_eq!(report.reason(Action::Retrieve), None);
    }

    /// Without this the press is queued but the window stays asleep, and the
    /// action does not happen until something else wakes it.
    #[test]
    fn queuing_an_event_wakes_the_ui_thread() {
        let (shell, wakes) = shell();
        shell.trigger(Action::Send);
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn polling_drains_what_it_returns() {
        let (shell, _) = shell();
        shell.trigger(Action::Send);

        assert_eq!(shell.poll(), vec![ShellEvent::Trigger(Action::Send)]);
        assert!(shell.poll().is_empty(), "an event must not fire twice");
    }

    #[test]
    fn an_empty_set_unregisters_everything() {
        let (shell, _) = shell();
        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(hotkey("Ctrl+Alt+F9"))).unwrap();
        shell.apply_hotkeys(hotkeys);
        shell.poll();

        shell.apply_hotkeys(Hotkeys::default());
        assert_eq!(shell.registered(), Hotkeys::default());
    }

    #[test]
    fn a_rejected_binding_is_not_treated_as_registered() {
        let (shell, _) = shell();
        shell.set_unavailable(vec![hotkey("Ctrl+Alt+F9")]);

        let mut hotkeys = Hotkeys::default();
        hotkeys.set(Action::Send, Some(hotkey("Ctrl+Alt+F9"))).unwrap();
        shell.apply_hotkeys(hotkeys);

        assert_eq!(shell.registered().send, None);
    }
}
