//! The real updater: GitHub over HTTPS, on threads of its own.
//!
//! Deliberately thin. Everything worth reasoning about — which release is
//! newest, whether a download is the one that was published, and the reversible
//! swap of one executable for another — lives in `update.rs` where it is
//! portable and tested. What is left here is the request, the thread around it,
//! and the order the two are put in, which is the part that genuinely cannot be
//! exercised without a network.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::shell::Waker;
use crate::update::{self, Release, UpdateEvent, Updater};

/// GitHub refuses a request without one, and asks that it identify the
/// application rather than the library.
const USER_AGENT: &str = concat!("WinSend/", env!("CARGO_PKG_VERSION"));

/// Long enough for a slow connection on a venue's network, short enough that a
/// thread cannot sit there for the rest of the session. Nothing waits on this,
/// so the only cost of it expiring is that no indicator appears.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A ceiling on the download, well clear of the few megabytes the executable
/// actually is. Not a security measure so much as a refusal to read an
/// unbounded response into memory on a machine that is mid-broadcast.
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;

pub struct GithubUpdater {
    waker: Waker,
    sender: Sender<UpdateEvent>,
    /// Behind a mutex because `poll` takes `&self`, the way the rest of the
    /// seams do. It is only ever locked by the UI thread.
    events: Mutex<Receiver<UpdateEvent>>,
}

impl GithubUpdater {
    pub fn new(waker: Waker) -> Self {
        let (sender, receiver) = channel();
        Self { waker, sender, events: Mutex::new(receiver) }
    }

    /// Run `work` on a thread of its own and deliver whatever it produces.
    ///
    /// The thread lives exactly as long as the operation. Answering `None` is
    /// how the check stays silent about a failure: no event is sent and the UI
    /// is never woken, so nothing appears.
    fn spawn(&self, work: impl FnOnce() -> Option<UpdateEvent> + Send + 'static) {
        let sender = self.sender.clone();
        let waker = Arc::clone(&self.waker);

        std::thread::spawn(move || {
            if let Some(event) = work() {
                // Send before waking, or the UI can run a frame and find the
                // queue still empty.
                if sender.send(event).is_ok() {
                    waker();
                }
            }
        });
    }
}

impl Updater for GithubUpdater {
    fn check(&self) {
        self.spawn(|| {
            // Every failure answers `None` and says nothing. No network, GitHub
            // down, rate limited, a response that will not parse: none of these
            // is something the user asked about or can act on, and none is
            // worth a line of the status strip during a broadcast.
            let json = fetch(update::releases_url()).ok()?;
            let newest = update::newest_release(&json).ok()?;
            update::newer_than_current(newest).map(UpdateEvent::Available)
        });
    }

    fn install(&self, release: Release) {
        // Failures here are the opposite: the user asked for this and is
        // waiting, so every one of them is reported.
        self.spawn(move || {
            Some(match install(&release) {
                Ok(()) => UpdateEvent::Installed,
                Err(why) => UpdateEvent::Failed(why),
            })
        });
    }

    fn poll(&self) -> Vec<UpdateEvent> {
        let Ok(events) = self.events.lock() else {
            return Vec::new();
        };
        events.try_iter().collect()
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent()
}

fn fetch(url: &str) -> Result<String, String> {
    agent()
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| format!("could not download the update: {e}"))?
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(|e| format!("the download did not complete: {e}"))
}

/// Fetch the release, check it, and put it in place.
///
/// The digest is checked before anything on disk is touched, so a download
/// that is not what the release published costs nothing but the bandwidth.
fn install(release: &Release) -> Result<(), String> {
    let current = std::env::current_exe()
        .map_err(|e| format!("could not find this application on disk: {e}"))?;

    let bytes = download(&release.url)?;
    if !update::digest_matches(&bytes, &release.digest) {
        return Err(
            "the download did not match the checksum published with the release, \
             so it has not been installed"
                .to_string(),
        );
    }

    let downloaded = update::download_path(&current);
    std::fs::write(&downloaded, &bytes)
        .map_err(|e| format!("could not write the update beside the application: {e}"))?;

    if let Err(why) = update::swap_in_place(&current, &downloaded) {
        // The swap puts the original back itself; this is only the half-written
        // download, which would otherwise sit there until the next start.
        let _ = std::fs::remove_file(&downloaded);
        return Err(why);
    }

    Ok(())
}
