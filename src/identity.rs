//! Re-finding the user's confirmed Zoom window on later launches.
//!
//! Window handles die with the process that owned them, so what gets persisted
//! is a description of the window rather than a pointer to it. Resolution is
//! deliberately conservative: it would rather report `Ambiguous` and ask than
//! silently move the wrong window during a live broadcast.

use serde::{Deserialize, Serialize};

use crate::platform::WindowCandidate;

/// The identifying detail persisted for the window the user confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowIdentity {
    pub process_name: String,
    pub class_name: String,
    pub title: String,
}

impl WindowIdentity {
    pub fn from_candidate(candidate: &WindowCandidate) -> Self {
        Self {
            process_name: candidate.process_name.clone(),
            class_name: candidate.class_name.clone(),
            title: candidate.title.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Found(u64),
    /// More than one window fits and none is a clear winner. The caller must
    /// ask the user to reconfirm rather than pick one.
    Ambiguous(Vec<u64>),
    NotFound,
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

/// Whether a window still looks like the one that was confirmed, ignoring the
/// title. Used to validate a remembered handle before trusting it: a handle can
/// be reused by the OS after its window dies, so existence alone is not enough.
pub fn matches_structurally(identity: &WindowIdentity, candidate: &WindowCandidate) -> bool {
    eq_ignore_case(&candidate.process_name, &identity.process_name)
        && eq_ignore_case(&candidate.class_name, &identity.class_name)
}

/// Resolve a persisted identity against the windows currently on screen.
///
/// Process and class are treated as the structural identity and must both
/// match. Title is only a tie-breaker, because Zoom reuses titles across its
/// main meeting window and the dual-monitor video window — the exact ambiguity
/// this whole flow exists to handle.
pub fn resolve(identity: &WindowIdentity, candidates: &[WindowCandidate]) -> Resolution {
    let structural: Vec<&WindowCandidate> = candidates
        .iter()
        .filter(|c| matches_structurally(identity, c))
        .collect();

    match structural.len() {
        0 => Resolution::NotFound,
        1 => Resolution::Found(structural[0].handle),
        _ => {
            // Several windows share the process and class. An exact title match
            // is the only thing that can break the tie safely.
            let exact: Vec<&&WindowCandidate> = structural
                .iter()
                .filter(|c| c.title == identity.title)
                .collect();

            if exact.len() == 1 {
                Resolution::Found(exact[0].handle)
            } else {
                Resolution::Ambiguous(structural.iter().map(|c| c.handle).collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Bounds;

    fn candidate(handle: u64, process: &str, class: &str, title: &str) -> WindowCandidate {
        WindowCandidate {
            handle,
            process_name: process.to_string(),
            class_name: class.to_string(),
            title: title.to_string(),
            bounds: Bounds::new(0, 0, 1920, 1080),
            monitor_id: r"\\.\DISPLAY1".to_string(),
            likely_zoom: true,
            minimized: false,
            topmost: false,
            own_process: false,
            cloaked: false,
            z_order: 0,
        }
    }

    fn identity(process: &str, class: &str, title: &str) -> WindowIdentity {
        WindowIdentity {
            process_name: process.to_string(),
            class_name: class.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn finds_the_only_structural_match() {
        let candidates = vec![
            candidate(1, "Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            candidate(2, "chrome.exe", "Chrome_WidgetWin_1", "GitHub"),
        ];
        let result = resolve(
            &identity("Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            &candidates,
        );
        assert_eq!(result, Resolution::Found(1));
    }

    #[test]
    fn matches_process_and_class_case_insensitively() {
        let candidates = vec![candidate(7, "ZOOM.EXE", "ZPContentViewWndClass", "Zoom Meeting")];
        let result = resolve(
            &identity("zoom.exe", "zpcontentviewwndclass", "Zoom Meeting"),
            &candidates,
        );
        assert_eq!(result, Resolution::Found(7));
    }

    #[test]
    fn title_breaks_a_tie_between_same_class_windows() {
        // The real Zoom case: main window and video window, same class.
        let candidates = vec![
            candidate(1, "Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            candidate(2, "Zoom.exe", "ZPContentViewWndClass", "Zoom Workplace"),
        ];
        let result = resolve(
            &identity("Zoom.exe", "ZPContentViewWndClass", "Zoom Workplace"),
            &candidates,
        );
        assert_eq!(result, Resolution::Found(2));
    }

    #[test]
    fn identical_titles_are_ambiguous_rather_than_guessed() {
        let candidates = vec![
            candidate(1, "Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            candidate(2, "Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
        ];
        let result = resolve(
            &identity("Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            &candidates,
        );
        assert_eq!(result, Resolution::Ambiguous(vec![1, 2]));
    }

    #[test]
    fn title_drift_alone_still_resolves() {
        // Zoom renames the window mid-meeting; class and process still pin it.
        let candidates = vec![candidate(
            5,
            "Zoom.exe",
            "ZPContentViewWndClass",
            "Zoom Meeting — Joe",
        )];
        let result = resolve(
            &identity("Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            &candidates,
        );
        assert_eq!(result, Resolution::Found(5));
    }

    #[test]
    fn class_change_is_not_found_rather_than_a_wrong_match() {
        // A Zoom update renames the class. We must not fall back to "some other
        // Zoom.exe window" and full-screen the main meeting window.
        let candidates = vec![candidate(1, "Zoom.exe", "ZPNewVideoWndClass", "Zoom Meeting")];
        let result = resolve(
            &identity("Zoom.exe", "ZPContentViewWndClass", "Zoom Meeting"),
            &candidates,
        );
        assert_eq!(result, Resolution::NotFound);
    }

    #[test]
    fn no_candidates_is_not_found() {
        let result = resolve(&identity("Zoom.exe", "ZPContentViewWndClass", "Zoom"), &[]);
        assert_eq!(result, Resolution::NotFound);
    }
}
