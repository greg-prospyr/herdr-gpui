//! A fresh, cancellable preflight owned by one close dialog. It informs and
//! never gates: closing ends terminals, and the checkout keeps its files,
//! commits, and branch, so the dialog closes at once whatever it finds.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use gpui::App;
use herdr_client::protocol::{ClientShellSnapshot, ClientShellWorktree};

use super::workspace::WorkspaceTarget;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Checkout {
    id: String,
    worktree: Option<ClientShellWorktree>,
    branch: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reports_only_mention_work_that_stays_on_disk() {
        let report = |dirty, unpushed| Report { dirty, unpushed };
        assert_eq!(report(false, false).note(), None);
        assert_eq!(
            report(true, false).note(),
            Some("Uncommitted files stay on disk.")
        );
        assert_eq!(
            report(false, true).note(),
            Some("Unpushed commits stay on disk.")
        );
        assert!(report(true, true).note().unwrap().contains("and"));
    }

    #[test]
    fn changed_sibling_checkout_invalidates_group_consent() {
        let mut snapshot = crate::sidebar::layout_tests::snapshot(7);
        let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]);
        let check = CloseCheck::fixture(&snapshot, &target, Some(Report::default()));
        assert!(check.current(&snapshot, &target));
        snapshot.workspaces[4].branch = Some("replacement".into());
        assert!(!check.current(&snapshot, &target));
    }

    #[test]
    fn a_disconnected_worker_says_nothing_and_drop_cancels() {
        let snapshot = crate::sidebar::layout_tests::snapshot(7);
        let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]);
        let mut check = CloseCheck::fixture(&snapshot, &target, None);
        assert!(check.poll());
        assert_eq!(check.report.as_ref().and_then(Report::note), None);
        let cancelled = check.cancelled.clone();
        drop(check);
        assert!(cancelled.load(Ordering::Relaxed));
    }
}

#[derive(Default)]
pub(super) struct Report {
    pub dirty: bool,
    pub unpushed: bool,
}

impl Report {
    /// What the dialog mentions, if anything: work that stays on disk. A
    /// status that could not be read says nothing.
    pub fn note(&self) -> Option<&'static str> {
        match (self.dirty, self.unpushed) {
            (true, true) => Some("Uncommitted files and unpushed commits stay on disk."),
            (true, false) => Some("Uncommitted files stay on disk."),
            (false, true) => Some("Unpushed commits stay on disk."),
            (false, false) => None,
        }
    }
}

pub(super) struct CloseCheck {
    checkouts: Vec<Checkout>,
    receiver: mpsc::Receiver<Report>,
    cancelled: Arc<AtomicBool>,
    pub report: Option<Report>,
}

impl Drop for CloseCheck {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn checkouts(snapshot: &ClientShellSnapshot, target: &WorkspaceTarget) -> Vec<Checkout> {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| target.close_members.contains(&workspace.workspace_id))
        .map(|workspace| Checkout {
            id: workspace.workspace_id.clone(),
            worktree: workspace.worktree.clone(),
            branch: workspace.branch.clone(),
        })
        .collect()
}

impl CloseCheck {
    #[cfg(test)]
    pub(super) fn fixture(
        snapshot: &ClientShellSnapshot,
        target: &WorkspaceTarget,
        report: Option<Report>,
    ) -> Self {
        let (_, receiver) = mpsc::sync_channel(1);
        Self {
            checkouts: checkouts(snapshot, target),
            receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
            report,
        }
    }

    pub fn start(
        snapshot: &ClientShellSnapshot,
        target: &WorkspaceTarget,
        local: bool,
        cx: &App,
    ) -> Self {
        let checkouts = checkouts(snapshot, target);
        let inputs: Vec<_> = checkouts
            .iter()
            .take(128)
            .map(|checkout| {
                crate::pull_request::repository_input(
                    checkout.worktree.as_ref(),
                    checkout.branch.as_deref(),
                )
            })
            .collect();
        let mut report = Report::default();
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = cancelled.clone();
        cx.background_executor()
            .spawn(async move {
                let deadline = Instant::now() + Duration::from_secs(15);
                for input in inputs {
                    if cancellation.load(Ordering::Relaxed) {
                        return;
                    }
                    let status = input.and_then(|input| {
                        if !local {
                            return Err(crate::Error::PrUntrustedEndpoint);
                        }
                        crate::git::close_status(&input, deadline, &|| {
                            cancellation.load(Ordering::Relaxed)
                        })
                    });
                    if let Ok((dirty, unpushed)) = status {
                        report.dirty |= dirty;
                        report.unpushed |= unpushed;
                    }
                }
                let _ = sender.send(report);
            })
            .detach();
        Self {
            checkouts,
            receiver,
            cancelled,
            report: None,
        }
    }

    pub fn poll(&mut self) -> bool {
        if self.report.is_some() {
            return false;
        }
        match self.receiver.try_recv() {
            Ok(report) => self.report = Some(report),
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => self.report = Some(Report::default()),
        }
        true
    }

    pub fn current(&self, snapshot: &ClientShellSnapshot, target: &WorkspaceTarget) -> bool {
        self.checkouts == checkouts(snapshot, target)
    }
}
