#![allow(clippy::unwrap_used)]

use super::{WorkspaceAction, WorkspaceTarget, state::Deletion};
use crate::{HerdrWindow, dialog_input::DialogInput, sidebar};
use herdr_client::Method;

/// The workspace a menu currently targets, for tests outside this module.
pub(crate) fn target_id(view: &HerdrWindow) -> Option<&str> {
    view.menu.target.as_ref().map(|target| target.id.as_str())
}

// Drives the workspace menu for `endpoint::lifecycle_tests`, which needs POSIX
// sockets and processes and is therefore compiled there only.
#[cfg(unix)]
pub(crate) fn submit_focus_change(
    view: &mut HerdrWindow,
    method: Method,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<HerdrWindow>,
) {
    let action = match method {
        Method::WorkspaceClose => WorkspaceAction::Close,
        Method::WorktreeCreate => WorkspaceAction::NewWorktree,
        Method::WorktreeOpen => WorkspaceAction::OpenWorktree,
        Method::WorktreeRemove => WorkspaceAction::DeleteWorktree,
        _ => panic!("unexpected fixture action"),
    };
    let snapshot = std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap());
    snapshot.workspaces = sidebar::layout_tests::snapshot(7).workspaces;
    let index = if action == WorkspaceAction::DeleteWorktree {
        4
    } else {
        3
    };
    let target = WorkspaceTarget::new(snapshot, &snapshot.workspaces[index]);
    let close_check = (action == WorkspaceAction::Close).then(|| {
        super::workspace_close::CloseCheck::fixture(
            snapshot,
            &target,
            Some(super::workspace_close::Report {
                dirty: true,
                unpushed: true,
            }),
        )
    });
    view.open_menu(window, cx);
    view.menu.target = Some(target);
    view.menu.page = Some(super::Page::Dialog(action));
    if let Some(check) = close_check {
        view.menu.close_check = Some(check);
    }
    if action == WorkspaceAction::DeleteWorktree {
        view.menu.deletion = Some(Deletion {
            pending: None,
            path: Some("/fixture/checkout".into()),
            force: false,
        });
    }
    if action == WorkspaceAction::OpenWorktree {
        use gpui::AppContext;
        let mut picker =
            super::worktree_open::Picker::new(cx.new(crate::search_input::SearchInput::new));
        picker.pending = Some("list".into());
        view.menu.worktree_open = Some(picker);
        view.menu.apply_worktree_list_response("list", Ok(serde_json::json!({"result": {
            "type": "worktree_list", "source": {"repo_key":"/fixture/agent-launcher/.git", "repo_name":"agent-launcher", "source_workspace_id":"w3"},
            "worktrees": [{"path": "/endpoint/existing checkout ",
                "label": "existing", "is_bare": false, "is_prunable": false, "is_detached": true}]
        }})));
    }
    view.submit_workspace_dialog(window, cx);
    assert!(view.menu.error.is_none() && view.local_error.is_none());
    // A creation waits for its correlated response in the open dialog; a
    // removal is queued and its dialog closes, leaving the id on the window.
    let pending = match action {
        WorkspaceAction::DeleteWorktree => {
            assert!(view.menu.page.is_none());
            view.removal.as_ref().unwrap().pending.clone()
        }
        WorkspaceAction::NewWorktree | WorkspaceAction::OpenWorktree => view.menu.creation.clone(),
        _ => None,
    };
    if pending.is_some() {
        let inbox = view.endpoints[view.selected_endpoint]
            .connection
            .inbox
            .lock()
            .unwrap();
        assert_eq!(
            inbox.dialog_response.as_ref().map(|(id, _)| id),
            pending.as_ref()
        );
    }
    view.dismiss_menu(window, cx);
}

#[gpui::test]
fn close_submits_at_once_and_only_notes_work_left_on_disk(cx: &mut gpui::TestAppContext) {
    use super::workspace_close::{CloseCheck, Report};
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    let submit = |cx: &mut gpui::VisualTestContext, report: Option<Report>| {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.live.status = crate::state::ConnectionStatus::Connected;
                let snapshot = std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap());
                snapshot.workspaces = sidebar::layout_tests::snapshot(7).workspaces;
                view.open_workspace_menu("w3", Default::default(), window, cx);
                view.menu.page = Some(super::Page::Dialog(WorkspaceAction::Close));
                let snapshot = view.live.snapshot.as_ref().unwrap();
                let target = view.menu.target.as_ref().unwrap();
                view.menu.close_check = Some(CloseCheck::fixture(snapshot, target, report));
                assert!(view.menu.input.is_none(), "nothing to type");
            });
            window.draw(cx).clear(cx);
        });
    };
    // A dirty, unpushed checkout is mentioned, quietly, above the buttons.
    submit(
        cx,
        Some(Report {
            dirty: true,
            unpushed: true,
        }),
    );
    let panel = cx.debug_bounds("menu-panel").unwrap();
    let note = cx.debug_bounds("close-git-status").unwrap();
    let button = cx.debug_bounds("dialog-submit").unwrap();
    assert!(panel.contains(&note.origin));
    assert!(note.bottom() <= button.top());
    // Neither found work nor a check still running holds the close back: it
    // goes straight to the connection, which the fixture lacks.
    for report in [
        Some(Report {
            dirty: true,
            unpushed: true,
        }),
        None,
    ] {
        submit(cx, report);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.submit_workspace_dialog(window, cx);
                assert!(view.menu.error.is_some(), "the close was attempted");
                view.dismiss_menu(window, cx);
                assert!(view.menu.close_check.is_none());
            })
        });
    }
}

#[gpui::test]
fn signed_out_workspace_has_no_github_section_or_requests(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.live.status = crate::state::ConnectionStatus::Connected;
            view.open_workspace_menu("w3", Default::default(), window, cx);
            view.refresh_workspace_pr();
            assert!(!view.menu.pr.loading);
            assert!(view.menu.pr.message.is_none());
            assert!(!view.menu.github.busy());
            assert!(!view.menu.github.loading_profile());
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("workspace-pr").is_none());
}

#[gpui::test]
fn workspace_dialogs_and_prs_are_fenced_by_host_and_generation(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.menu.github = crate::github::Auth::connected_fixture();
            view.live.status = crate::state::ConnectionStatus::Connected;
            view.endpoints[0].live = view.live.clone();
            view.endpoints[0].live.supports_surface = true;
            let mut remote = crate::endpoint::Endpoint::new(
                "ssh:fixture".into(),
                "Remote".into(),
                herdr_client::ConnectTarget::Ssh {
                    target: "unused".into(),
                    session: "default".into(),
                },
                true,
            );
            // Identical boot/workspace IDs must not make different hosts interchangeable.
            remote.live = view.live.clone();
            remote.live.local_daemon_peer = true;
            view.endpoints.push(remote);
            view.open_workspace_menu("w3", Default::default(), window, cx);
            view.open_workspace_dialog(WorkspaceAction::Rename, window, cx);
            view.endpoints[0].generation += 1;
            view.submit_workspace_dialog(window, cx);
            assert_eq!(
                view.menu.error.as_deref(),
                Some("The selected connection changed or is not ready. Cancel and try again.")
            );
            assert!(view.select_endpoint("ssh:fixture", cx));
            assert!(view.menu.page.is_none());
            assert!(view.menu.input.is_none());
            view.open_workspace_menu("w3", Default::default(), window, cx);
            // A saved SSH host resolves its repository on that host, so its
            // lookup is accepted and scoped to it rather than to local Git.
            assert_eq!(
                view.pr_origin(),
                Some(crate::pull_request::Origin::Ssh("unused".into()))
            );
            assert!(view.menu.pr.message.is_none());
            assert!(view.menu.pr.loading);
            view.open_workspace_dialog(WorkspaceAction::DeleteWorktree, window, cx);
            assert!(view.select_endpoint(crate::endpoint::LOCAL, cx));
            assert!(view.menu.deletion.is_none());
        });
    });
}

pub(crate) fn check_pr_fences(view: &gpui::Entity<HerdrWindow>, cx: &mut gpui::VisualTestContext) {
    use std::sync::{Arc, Mutex};
    cx.update(|_, cx| {
        view.update(cx, |view, _| {
            view.menu.github = crate::github::Auth::connected_fixture();
            let snapshot = view.live.snapshot.clone();
            let inbox = view.endpoints[view.selected_endpoint]
                .connection
                .inbox
                .clone();
            let status = view.live.status;
            let target_id = view.menu.target.as_ref().unwrap().id.clone();
            for change in 0..5 {
                view.live.snapshot = snapshot.clone();
                view.live.status = status;
                view.endpoints[view.selected_endpoint].connection.inbox = inbox.clone();
                view.menu.pr_connection = Some(Arc::downgrade(&inbox));
                view.menu.pr.value = Some(crate::pull_request::fixture().unwrap());
                view.menu.pr.message = None;
                let snapshot = Arc::make_mut(view.live.snapshot.as_mut().unwrap());
                match change {
                    0 => snapshot.boot_id = "restarted".into(),
                    1 => snapshot
                        .workspaces
                        .retain(|workspace| workspace.workspace_id != target_id),
                    2 => {
                        snapshot
                            .workspaces
                            .iter_mut()
                            .find(|workspace| workspace.workspace_id == target_id)
                            .unwrap()
                            .branch = Some("other".into())
                    }
                    3 => {
                        view.endpoints[view.selected_endpoint].connection.inbox =
                            Arc::new(Mutex::new(Default::default()))
                    }
                    _ => view.live.status = crate::state::ConnectionStatus::Detached,
                }
                assert!(view.update_workspace_pr());
                assert!(view.menu.pr.value.is_none());
            }
            view.live.snapshot = snapshot;
            view.live.status = status;
            view.endpoints[view.selected_endpoint].connection.inbox = inbox;
            view.menu.pr_connection = None;
            view.menu.pr.clear();
            let connection_target = view.endpoints[view.selected_endpoint]
                .connection
                .target
                .clone();
            let local_peer = view.live.local_daemon_peer;
            let supports_workspace_get = view.live.supports_workspace_get;
            view.live.supports_workspace_get = true;
            for target in [
                herdr_client::ConnectTarget::Local,
                herdr_client::ConnectTarget::Socket("/local-or-forwarded.sock".into()),
            ] {
                view.endpoints[view.selected_endpoint].connection.target = target;
                view.live.local_daemon_peer = false;
                view.refresh_workspace_pr();
                assert!(
                    view.menu
                        .pr
                        .message
                        .as_deref()
                        .unwrap()
                        .contains("requires your owned local session socket")
                );
                view.live.local_daemon_peer = true;
                view.refresh_workspace_pr();
                // Menu open is cache-only, even on a newer daemon.
                assert!(view.menu.pr.loading);
                assert!(view.menu.pr.message.is_none());
            }
            view.live.supports_workspace_get = false;
            view.refresh_workspace_pr();
            assert!(
                view.menu.pr.loading,
                "older local daemon uses Git registry worker"
            );
            assert!(
                view.live.dialog_response.is_none(),
                "no workspace.get request or dialog slot registration"
            );
            assert!(
                view.menu.pr_connection.is_some(),
                "fallback keeps reconnect fence"
            );
            view.endpoints[view.selected_endpoint].connection.target = connection_target;
            view.live.local_daemon_peer = local_peer;
            view.live.supports_workspace_get = supports_workspace_get;
            view.menu.pr.clear();
        })
    });
}

pub(crate) fn check_menu_interactions(
    view: &gpui::Entity<HerdrWindow>,
    cx: &mut gpui::VisualTestContext,
) {
    use gpui::{Modifiers, point, px};
    let selection = |view: &HerdrWindow| {
        if view.menu.page == Some(super::Page::Workspace) {
            view.menu.workspace_selected.and_then(|selected| {
                view.workspace_menu_actions()
                    .iter()
                    .position(|action| *action == selected)
            })
        } else {
            view.menu.selected
        }
    };
    let (page, count, first, second) = cx.update(|_, cx| {
        let view = view.read(cx);
        assert_eq!(view.menu.selected, None);
        if view.menu.page == Some(super::Page::Workspace) {
            (
                super::Page::Workspace,
                view.workspace_items().len(),
                "workspace-menu-Rename",
                "workspace-menu-Close group",
            )
        } else {
            (
                super::Page::Menu,
                view.menu_items().len(),
                "menu-settings",
                "menu-shortcuts",
            )
        }
    });
    cx.simulate_keystrokes("enter");
    cx.update(|_, cx| {
        assert!(view.read(cx).menu.page == Some(page));
        assert_eq!(view.read(cx).menu.selected, None);
        assert!(view.read(cx).menu.input.is_none());
    });
    let first = cx.debug_bounds(first).unwrap().center();
    let second = cx.debug_bounds(second).unwrap().center();
    let outside = point(px(790.), px(590.));
    for (position, selected) in [(second, Some(1)), (first, Some(0)), (outside, None)] {
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(selection(view.read(cx)), selected);
        });
    }
    // Leaving the hovered row also leaves Enter inert.
    cx.simulate_keystrokes("enter");
    cx.update(|_, cx| assert!(view.read(cx).menu.page == Some(page)));
    for (keys, selected) in [
        ("up", count - 1),
        ("down", 0),
        ("up", count - 1),
        ("down down", 1),
    ] {
        cx.simulate_keystrokes(keys);
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(selection(view.read(cx)), Some(selected));
        });
    }
    cx.simulate_mouse_move(first, None, Modifiers::default());
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(selection(view.read(cx)), Some(0));
    });
    // Keyboard selection replaces hover even while the pointer stays on the first row.
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(selection(view.read(cx)), Some(1));
    });
    cx.simulate_mouse_move(second, None, Modifiers::default());
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(selection(view.read(cx)), Some(1));
    });
    cx.simulate_mouse_move(outside, None, Modifiers::default());
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(selection(view.read(cx)), None);
    });
    cx.simulate_keystrokes("down");
    cx.update(|_, cx| assert_eq!(selection(view.read(cx)), Some(0)));
    cx.simulate_mouse_move(second, None, Modifiers::default());
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(selection(view.read(cx)), Some(1));
    });
    cx.simulate_keystrokes("enter");
    cx.update(|_, cx| {
        assert!(
            view.read(cx).menu.page
                == Some(if page == super::Page::Workspace {
                    super::Page::Dialog(WorkspaceAction::Close)
                } else {
                    super::Page::Keybinds
                })
        );
    });
    cx.simulate_mouse_move(outside, None, Modifiers::default());
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            let anchor = view.menu.anchor;
            let target = view.menu.target.as_ref().map(|target| target.id.clone());
            view.dismiss_menu(window, cx);
            if let Some(target) = target {
                view.open_workspace_menu(&target, anchor, window, cx);
            } else {
                view.open_menu(window, cx);
            }
            assert_eq!(view.menu.selected, None);
        });
        window.draw(cx).clear(cx);
    });
}

/// The row menu follows the pointer, but the dialog it opens is a modal: it
/// centres over the window like the Herdr TUI's, whatever corner the menu was
/// opened from.
#[gpui::test]
fn workspace_dialogs_centre_on_the_window_rather_than_the_pointer(cx: &mut gpui::TestAppContext) {
    use gpui::{point, px};
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.simulate_resize(gpui::size(px(800.), px(600.)));
    let centre = point(px(400.), px(300.));
    for anchor in [point(px(120.), px(140.)), point(px(700.), px(520.))] {
        for action in [
            WorkspaceAction::NewWorktree,
            WorkspaceAction::DeleteWorktree,
        ] {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    let snapshot = std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap());
                    snapshot.workspaces = sidebar::layout_tests::snapshot(7).workspaces;
                    view.live.status = crate::state::ConnectionStatus::Connected;
                    view.menu.reset();
                    let id = if action == WorkspaceAction::NewWorktree {
                        "w3"
                    } else {
                        "w4"
                    };
                    view.open_workspace_menu(id, anchor, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            // The menu itself still opens where the pointer asked for it.
            let menu = cx.debug_bounds("menu-panel").unwrap();
            assert!(
                (menu.center() - centre).x.abs() > px(40.)
                    || (menu.center() - centre).y.abs() > px(40.),
                "{anchor:?}: {menu:?}"
            );
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.open_workspace_dialog(action, window, cx)
                });
                window.draw(cx).clear(cx);
            });
            let panel = cx.debug_bounds("menu-panel").unwrap();
            let offset = panel.center() - centre;
            assert!(
                offset.x.abs() <= px(1.) && offset.y.abs() <= px(1.),
                "{anchor:?} {action:?}: {panel:?}"
            );
            // The new worktree tabs keep a listing's width on every tab.
            let widest = if action == WorkspaceAction::NewWorktree {
                px(560.)
            } else {
                px(480.)
            };
            assert!(panel.size.width <= widest && panel.size.width >= px(400.));
        }
    }
}

/// The dialog chrome matches the other modals: sections stacked in reading
/// order inside the panel, and a right-aligned Cancel/submit row.
#[gpui::test]
fn workspace_dialog_sections_and_buttons_stay_inside_the_panel(cx: &mut gpui::TestAppContext) {
    use gpui::px;
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    for (width, height) in [(640., 400.), (1200., 780.)] {
        cx.simulate_resize(gpui::size(px(width), px(height)));
        for action in [
            WorkspaceAction::NewWorktree,
            WorkspaceAction::DeleteWorktree,
        ] {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    let snapshot = std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap());
                    snapshot.workspaces = sidebar::layout_tests::snapshot(7).workspaces;
                    snapshot.worktree_directory = "/endpoint/.herdr/worktrees".into();
                    view.live.status = crate::state::ConnectionStatus::Connected;
                    view.menu.reset();
                    let id = if action == WorkspaceAction::NewWorktree {
                        "w3"
                    } else {
                        "w4"
                    };
                    view.open_workspace_menu(id, Default::default(), window, cx);
                    view.open_workspace_dialog(action, window, cx);
                    if action == WorkspaceAction::DeleteWorktree {
                        // Ready to confirm: the daemon has named the
                        // checkout and nothing is in flight.
                        view.menu.deletion = Some(Deletion {
                            pending: None,
                            path: Some("/endpoint/.herdr/worktrees/agent-launcher/child".into()),
                            force: true,
                        });
                    } else {
                        // The creation waits on the daemon here, so its
                        // waiting note belongs to this frame too.
                        view.menu.creation = Some("create".into());
                    }
                    view.menu.error = Some("fixture error".into());
                });
                window.draw(cx).clear(cx);
            });
            let panel = cx.debug_bounds("menu-panel").unwrap();
            let cancel = cx.debug_bounds("dialog-cancel").unwrap();
            let submit = cx.debug_bounds("dialog-submit").unwrap();
            let error = cx.debug_bounds("dialog-error").unwrap();
            // Only a creation waits on the daemon: a removal is queued and
            // its dialog closes rather than reporting progress.
            let waiting = (action == WorkspaceAction::NewWorktree)
                .then(|| cx.debug_bounds("dialog-waiting").unwrap());
            // Creation drafts a branch and previews its checkout; deletion
            // confirms the one the daemon named, with nothing to type.
            let (field, subject) = if action == WorkspaceAction::NewWorktree {
                (
                    cx.debug_bounds("dialog-input").unwrap(),
                    cx.debug_bounds("dialog-checkout").unwrap(),
                )
            } else {
                // Deletion is confirmed by its button, with nothing to type.
                assert!(cx.update(|_, cx| view.read(cx).menu.input.is_none()));
                let path = cx.debug_bounds("dialog-path").unwrap();
                (path, path)
            };
            // The name comes first, on the same edge as the branch it names.
            let name = (action == WorkspaceAction::NewWorktree)
                .then(|| cx.debug_bounds("worktree-name").unwrap());
            if let Some(name) = name {
                assert!(name.bottom() <= field.top());
                assert_eq!(name.left(), field.left());
                assert_eq!(name.right(), field.right());
            }
            let parts: Vec<_> = [field, subject, cancel, submit, error]
                .into_iter()
                .chain(waiting)
                .chain(name)
                .collect();
            for part in &parts {
                assert!(
                    part.left() >= panel.left() && part.right() <= panel.right(),
                    "{action:?} at {width}: {part:?} escapes {panel:?}"
                );
                assert!(part.right() <= px(width));
                // A roomy window must not make any dialog scroll to its buttons.
                if height > 400. {
                    assert!(
                        part.top() >= panel.top() && part.bottom() <= panel.bottom(),
                        "{action:?} at {height}: {part:?} needs scrolling in {panel:?}"
                    );
                    assert!(part.bottom() <= px(height));
                }
            }
            // One gutter on both sides, and a trailing button row.
            assert_eq!(
                subject.left() - panel.left(),
                panel.right() - subject.right()
            );
            assert_eq!(field.right(), subject.right());
            assert!(cancel.right() <= submit.left());
            assert!(submit.right() < panel.right());
            assert!(error.bottom() <= cancel.top());
            // The checkout follows the branch it is derived from, and the
            // daemon's answer follows whichever one the dialog is about.
            assert!(field.bottom() <= subject.top() || field == subject);
            assert!(subject.bottom() <= error.top());
        }
    }
}

/// Opening the dialog prepares the same branch and checkout the terminal
/// client proposes, rather than an empty field.
#[gpui::test]
fn new_worktree_dialog_proposes_a_branch_and_previews_its_checkout(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            let snapshot = std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap());
            snapshot.workspaces = sidebar::layout_tests::snapshot(7).workspaces;
            snapshot.worktree_directory = "/endpoint/.herdr/worktrees".into();
            view.live.status = crate::state::ConnectionStatus::Connected;
            view.open_workspace_menu("w3", Default::default(), window, cx);
            view.open_workspace_dialog(WorkspaceAction::NewWorktree, window, cx);
            let branch = view.menu.input.as_ref().unwrap().text.clone();
            assert!(branch.starts_with("worktree/"), "{branch}");
            // Selected, so the first keystroke replaces the proposal.
            assert_eq!(view.menu.input.as_ref().unwrap().selection, 0..branch.len());
            assert_eq!(
                view.checkout_preview(),
                format!(
                    "/endpoint/.herdr/worktrees/agent-launcher/{}",
                    crate::worktree::branch_to_path_slug(&branch)
                )
            );
            // A blank field defers to the daemon instead of guessing a path.
            view.menu.input = Some(DialogInput::new("  ".into()));
            assert_eq!(view.checkout_preview(), "The daemon names the checkout.");
            view.menu.input = Some(DialogInput::new("feature/Login v2".into()));
            assert_eq!(
                view.checkout_preview(),
                "/endpoint/.herdr/worktrees/agent-launcher/feature-login-v2"
            );
            // Without a reported worktree directory no path is invented.
            std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap())
                .worktree_directory
                .clear();
            assert_eq!(view.checkout_preview(), "The daemon chooses the checkout.");
        });
    });
}

/// The daemon switches only its own session, so the client follows the
/// created checkout itself; failures stay visible in the open dialog.
#[gpui::test]
fn worktree_creation_reports_failures_and_follows_the_created_checkout(
    cx: &mut gpui::TestAppContext,
) {
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            let snapshot = std::sync::Arc::make_mut(view.live.snapshot.as_mut().unwrap());
            snapshot.workspaces = sidebar::layout_tests::snapshot(7).workspaces;
            view.live.status = crate::state::ConnectionStatus::Connected;
            view.open_workspace_menu("w3", Default::default(), window, cx);
            view.open_workspace_dialog(WorkspaceAction::NewWorktree, window, cx);
            view.menu.creation = Some("create".into());
            let dialog = Some(super::Page::Dialog(WorkspaceAction::NewWorktree));

            view.apply_creation_response(
                Ok(serde_json::json!({"error":{"code":"worktree_create_failed","message":"branch already checked out"}})),
                window,
                cx,
            );
            assert!(
                view.menu
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("branch already checked out")
            );
            assert_eq!(view.menu.page, dialog);
            assert!(view.pending_navigation.is_none());

            view.apply_creation_response(
                Ok(serde_json::json!({"result":{"type":"worktree_list","worktrees":[]}})),
                window,
                cx,
            );
            assert!(
                view.menu
                    .error
                    .as_deref()
                    .unwrap()
                    .starts_with("Unexpected daemon response")
            );
            assert_eq!(view.menu.page, dialog);

            view.apply_creation_response(
                Err(std::sync::Arc::new(crate::Error::Client(
                    herdr_client::Error::Disconnected,
                ))),
                window,
                cx,
            );
            assert_eq!(view.menu.page, dialog);
            assert!(view.pending_navigation.is_none());

            // Only the correlated response closes the dialog and navigates.
            let created = serde_json::json!({"result":{"type":"worktree_created","workspace":{"workspace_id":"w6"},"tab":{"tab_id":"t9"}}});
            view.collapsed_repos
                .insert(sidebar::layout_tests::REPO_KEY.to_owned());
            view.menu.creation = Some("create".into());
            view.live.dialog_response = Some(("unrelated".into(), Some(Ok(created.clone()))));
            view.update_workspace_dialog(window, cx);
            assert_eq!(view.menu.page, dialog);
            assert_eq!(view.menu.creation.as_deref(), Some("create"));

            view.live.dialog_response = Some(("create".into(), Some(Ok(created))));
            view.update_workspace_dialog(window, cx);
            assert!(view.menu.page.is_none());
            assert!(view.menu.creation.is_none());
            assert_eq!(
                view.pending_navigation,
                Some(crate::NavigationTarget::Workspace("w6".into()))
            );
            // A folded group cannot hide the checkout that was just created.
            assert!(view.collapsed_repos.is_empty());
        });
    });
}

#[test]
fn deletion_schema_and_target_validation() {
    let mut snapshot = sidebar::layout_tests::snapshot(7);
    let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[4]);
    assert_eq!(
        target
            .request(&snapshot, WorkspaceAction::DeleteWorktree, "")
            .unwrap(),
        (
            Method::WorktreeRemove,
            serde_json::json!({"workspace_id":"w4", "force":false, "trust_repository":false})
        )
    );
    for index in [0, 3] {
        assert!(
            WorkspaceTarget::new(&snapshot, &snapshot.workspaces[index])
                .request(&snapshot, WorkspaceAction::DeleteWorktree, "")
                .is_err()
        );
    }
    snapshot.workspaces[4].worktree = None;
    assert!(
        target
            .request(&snapshot, WorkspaceAction::DeleteWorktree, "")
            .is_err()
    );
    snapshot.workspaces[4].worktree = target.worktree.clone();
    snapshot.boot_id = "replacement".into();
    assert!(
        target
            .request(&snapshot, WorkspaceAction::DeleteWorktree, "")
            .is_err()
    );
    snapshot.boot_id = target.boot_id.clone();
    snapshot.workspaces.remove(4);
    assert!(
        target
            .request(&snapshot, WorkspaceAction::DeleteWorktree, "")
            .is_err()
    );
}

#[gpui::test]
fn deletion_lookup_names_the_checkout_and_reports_errors(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let snapshot = sidebar::layout_tests::snapshot(7);
        let mut menu = super::MenuState::new(cx);
        menu.target = Some(WorkspaceTarget::new(&snapshot, &snapshot.workspaces[4]));
        menu.page = Some(super::Page::Dialog(WorkspaceAction::DeleteWorktree));
        menu.deletion = Some(Deletion { pending: Some("list".into()), path: None, force: false });
        let lookup = serde_json::json!({"result":{"type":"worktree_list", "worktrees":[{"open_workspace_id":"w4", "path":"/daemon/checkout", "is_linked_worktree":true, "is_bare":false}]}});
        menu.apply_deletion_response("unrelated", Ok(lookup.clone()));
        assert!(menu.deletion.as_ref().unwrap().path.is_none());
        menu.apply_deletion_response("list", Ok(lookup));
        let deletion = menu.deletion.as_ref().unwrap();
        assert_eq!(deletion.path.as_deref(), Some("/daemon/checkout"));
        assert!(deletion.ready());
        // The dialog only ever awaits the lookup, so a removal reply here is
        // not something it can act on.
        menu.deletion.as_mut().unwrap().pending = Some("stray".into());
        menu.apply_deletion_response("stray", Ok(serde_json::json!({"result":{"type":"worktree_removed", "workspace_id":"w4"}})));
        assert!(menu.error.as_ref().unwrap().starts_with("Unexpected daemon response"));
        // A refused lookup keeps the daemon's own code and message.
        menu.deletion.as_mut().unwrap().pending = Some("retry".into());
        menu.apply_deletion_response("retry", Ok(serde_json::json!({"error":{"code":"worktree_list_failed", "message":"not a repository"}})));
        assert_eq!(menu.error.as_deref(), Some("worktree_list_failed: not a repository"));
        menu.deletion.as_mut().unwrap().pending = Some("broken".into());
        menu.apply_deletion_response("broken", Err(std::sync::Arc::new(crate::Error::Client(herdr_client::Error::UnsupportedMethod))));
        assert_eq!(menu.error.as_deref(), Some("method not advertised by endpoint"));
        // A reply arriving after the menu closed changes nothing.
        menu.deletion = Some(Deletion { pending: Some("late".into()), path: None, force: false });
        menu.reset();
        menu.apply_deletion_response("late", Err(std::sync::Arc::new(crate::Error::Client(herdr_client::Error::Disconnected))));
        assert!(menu.deletion.is_none());
        assert!(menu.error.is_none());
    });
}

/// Confirming a removal closes the popover, because the daemon drops the
/// workspace from its own snapshot when the removal lands. A refusal still
/// has to reach the user, and a dirty checkout arms the next dialog with
/// force instead of repeating the same refusal.
#[gpui::test]
fn queued_removal_closes_the_dialog_and_reports_refusals(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.live.status = crate::state::ConnectionStatus::Connected;
            let boot_id = view.live.snapshot.as_ref().unwrap().boot_id.clone();
            let endpoint = (
                view.selection_epoch,
                view.endpoints[view.selected_endpoint].generation,
            );
            let removal = move |pending: &str| super::Removal {
                endpoint,
                boot_id: boot_id.clone(),
                workspace: "w4".into(),
                pending: Some(pending.into()),
                force: false,
            };
            view.removal = Some(removal("remove"));
            // Another dialog's reply leaves the removal in flight.
            view.live.dialog_response = Some(("other".into(), Some(Ok(serde_json::json!({"result":{}})))));
            view.update_workspace_dialog(window, cx);
            assert!(view.removal.as_ref().unwrap().pending.is_some());
            view.live.dialog_response = Some(("remove".into(), Some(Ok(serde_json::json!({"error":{"code":"dirty_worktree_requires_force", "message":"modified or untracked files"}})))));
            view.update_workspace_dialog(window, cx);
            let refused = view.removal.as_ref().unwrap();
            assert!(refused.force && refused.pending.is_none());
            assert_eq!(view.local_error.as_deref(), Some("Remove worktree: dirty_worktree_requires_force: modified or untracked files"));
            view.open_workspace_menu("w4", Default::default(), window, cx);
            view.open_workspace_dialog(WorkspaceAction::DeleteWorktree, window, cx);
            assert!(view.menu.deletion.as_ref().unwrap().force);
            view.dismiss_menu(window, cx);
            // An accepted removal leaves nothing behind for the next dialog.
            view.removal = Some(super::Removal { force: true, ..removal("forced") });
            view.local_error = None;
            view.live.dialog_response = Some(("forced".into(), Some(Ok(serde_json::json!({"result":{"type":"worktree_removed", "workspace_id":"w4"}})))));
            view.update_workspace_dialog(window, cx);
            assert!(view.removal.is_none() && view.local_error.is_none());
            // A reply from a replaced connection is not this removal's.
            view.removal = Some(removal("stale"));
            view.endpoints[view.selected_endpoint].generation += 1;
            view.update_workspace_dialog(window, cx);
            assert!(view.removal.is_none());
        })
    });
}

#[gpui::test]
fn pending_removal_shows_loading_only_on_its_worktree(cx: &mut gpui::TestAppContext) {
    for state in 0..7 {
        let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.live.status = crate::state::ConnectionStatus::Connected;
                view.removal = Some(super::Removal {
                    endpoint: (
                        view.selection_epoch,
                        view.endpoints[view.selected_endpoint].generation,
                    ),
                    boot_id: view.live.snapshot.as_ref().unwrap().boot_id.clone(),
                    workspace: "w4".into(),
                    pending: Some("remove".into()),
                    force: false,
                });
                match state {
                    1 => view.removal.as_mut().unwrap().pending = None,
                    2 => view.removal.as_mut().unwrap().boot_id = "stale".into(),
                    3 => view.removal.as_mut().unwrap().endpoint.1 += 1,
                    4 => {
                        view.live.dialog_response = Some((
                            "remove".into(),
                            Some(Ok(
                                serde_json::json!({"result":{"type":"worktree_removed"}}),
                            )),
                        ));
                        view.update_workspace_dialog(window, cx);
                    }
                    5 => view.live.status = crate::state::ConnectionStatus::Connecting,
                    6 => view.removal.as_mut().unwrap().endpoint.0 += 1,
                    _ => {}
                }
                cx.notify();
            });
        });
        cx.run_until_parked();
        let loading = cx.debug_bounds("worktree-removing");
        assert_eq!(loading.is_some(), state == 0);
        if let Some(loading) = loading {
            let row = cx.debug_bounds("row-sidebar-child").unwrap();
            assert!(row.contains(&loading.origin));
            assert!(loading.right() <= row.right() && loading.bottom() <= row.bottom());
        }
    }
}

/// The confirmation matches the Herdr TUI: one modal, no typed phrase. The
/// queued removal itself is exercised by the connected endpoint fixture.
#[gpui::test]
fn deletion_dialog_confirms_without_a_text_field(cx: &mut gpui::TestAppContext) {
    let (view, cx) = cx.add_window_view(sidebar::layout_tests::fixture_window);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.live.status = crate::state::ConnectionStatus::Connected;
            view.open_workspace_menu("w4", Default::default(), window, cx);
            view.open_workspace_dialog(WorkspaceAction::DeleteWorktree, window, cx);
            assert!(view.menu.input.is_none());
            view.menu.error = None;
            view.menu.deletion = Some(Deletion {
                pending: None,
                path: Some("/daemon/checkout".into()),
                force: false,
            });
            cx.notify();
        })
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("dialog-input").is_none());
    assert!(cx.debug_bounds("dialog-error").is_none());
    // The daemon path reads as its own block above right-aligned actions,
    // all of it inside the panel rather than clipped by it.
    let panel = cx.debug_bounds("menu-panel").unwrap();
    let path = cx.debug_bounds("dialog-path").unwrap();
    let cancel = cx.debug_bounds("dialog-cancel").unwrap();
    let submit = cx.debug_bounds("dialog-submit").unwrap();
    assert!(panel.contains(&path.origin) && path.right() <= panel.right());
    assert!(path.bottom() <= cancel.top() && path.bottom() <= submit.top());
    assert_eq!(cancel.top(), submit.top());
    assert!(cancel.right() < submit.left());
    assert!(submit.right() <= panel.right());
    assert!(submit.bottom() <= panel.bottom());
}

#[gpui::test]
fn deletion_fails_closed_on_lookup_and_does_not_force_generic_errors(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| {
        let snapshot = sidebar::layout_tests::snapshot(7);
        for response in [
            serde_json::json!({"result":{"type":"worktree_list", "worktrees":[]}}),
            serde_json::json!({"error":{"code":"worktree_remove_failed", "message":"is not a working tree"}}),
            serde_json::json!({"result":{"type":"unexpected"}}),
        ] {
            let mut menu = super::MenuState::new(cx);
            menu.target = Some(WorkspaceTarget::new(&snapshot, &snapshot.workspaces[4]));
            menu.deletion = Some(Deletion { pending: Some("id".into()), path: None, force: false });
            menu.apply_deletion_response("id", Ok(response));
            assert!(menu.error.is_some());
            let deletion = menu.deletion.as_ref().unwrap();
            assert!(!deletion.force);
            assert!(!deletion.ready());
        }
    });
}

#[gpui::test]
fn reset_drops_target_draft_composition_and_error(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let snapshot = sidebar::layout_tests::snapshot(7);
        let mut menu = super::MenuState::new(cx);
        menu.target = Some(WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]));
        menu.page = Some(super::Page::Dialog(WorkspaceAction::Rename));
        let mut input = DialogInput::new("draft".into());
        input.replace(None, "composition", true, None);
        menu.input = Some(input);
        menu.error = Some("old connection error".into());
        menu.reset();
        assert!(menu.page.is_none());
        assert!(menu.target.is_none());
        assert!(menu.input.is_none());
        assert!(menu.error.is_none());
    });
}

#[test]
fn actions_target_clicked_workspace_and_match_daemon_schemas() {
    let mut snapshot = sidebar::layout_tests::snapshot(7);
    snapshot.workspaces[0].branch = None;
    let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]);
    assert!(target.can_create());
    assert_eq!(target.close_label(), "Close group");
    assert_eq!(target.close_members, ["w3", "w4", "w5"]);
    assert_eq!(
        target
            .request(&snapshot, WorkspaceAction::Rename, "new label")
            .unwrap(),
        (
            Method::WorkspaceRename,
            serde_json::json!({"workspace_id": "w3", "label": "new label"})
        )
    );
    assert_eq!(
        target
            .request(&snapshot, WorkspaceAction::Close, "")
            .unwrap(),
        (
            Method::WorkspaceClose,
            serde_json::json!({"workspace_id": "w3", "close_group": true})
        )
    );
    for branch in ["", "  ", " feature/test "] {
        let (method, params) = target
            .request(&snapshot, WorkspaceAction::NewWorktree, branch)
            .unwrap();
        assert_eq!(method, Method::WorktreeCreate);
        let mut expected = serde_json::json!({"workspace_id": "w3", "base": "HEAD", "focus": true, "trust_repository": false});
        if !branch.trim().is_empty() {
            expected["branch"] = branch.trim().into();
        }
        assert_eq!(params, expected);
    }
    assert!(matches!(
        target.request(&snapshot, WorkspaceAction::NewWorktree, "config reload"),
        Err(crate::Error::InvalidBranchName)
    ));
    for index in [0, 4, 5] {
        let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[index]);
        assert_eq!(target.close_label(), "Close");
        assert_eq!(target.close_members.len(), 1);
        assert!(!target.can_create());
        assert!(
            target
                .request(&snapshot, WorkspaceAction::NewWorktree, "")
                .is_err()
        );
    }
    let mut standalone = snapshot.clone();
    standalone
        .workspaces
        .retain(|w| w.workspace_id != "w4" && w.workspace_id != "w5");
    let target = WorkspaceTarget::new(&standalone, &standalone.workspaces[3]);
    assert!(target.can_create());
    assert_eq!(target.close_label(), "Close");
}

#[test]
fn branch_only_workspace_can_create_until_git_identity_disappears() {
    let mut snapshot = sidebar::layout_tests::snapshot(7);
    snapshot.workspaces[3].worktree = None;
    snapshot.workspaces[3].branch = Some("main".into());
    let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]);
    assert!(target.can_create());
    assert!(!target.can_delete());
    assert_eq!(
        target
            .request(&snapshot, WorkspaceAction::NewWorktree, "feature/test")
            .unwrap(),
        (
            Method::WorktreeCreate,
            serde_json::json!({"workspace_id": "w3", "base": "HEAD", "focus": true,
                "trust_repository": false, "branch": "feature/test"})
        )
    );

    snapshot.workspaces[3].branch = None;
    assert!(!WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]).can_create());
    assert!(matches!(
        target.request(&snapshot, WorkspaceAction::NewWorktree, ""),
        Err(crate::Error::WorkspaceRepositoryChanged)
    ));

    snapshot.workspaces[3].branch = Some("main".into());
    snapshot.workspaces[3].worktree = snapshot.workspaces[4].worktree.clone();
    assert!(!WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]).can_create());
    assert!(matches!(
        target.request(&snapshot, WorkspaceAction::NewWorktree, ""),
        Err(crate::Error::WorkspaceRepositoryChanged)
    ));
}

#[test]
fn stale_boot_target_and_changed_close_members_are_rejected() {
    let mut snapshot = sidebar::layout_tests::snapshot(7);
    let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]);
    snapshot.boot_id = "replacement".into();
    for action in [
        WorkspaceAction::Rename,
        WorkspaceAction::Close,
        WorkspaceAction::NewWorktree,
    ] {
        assert!(target.request(&snapshot, action, "valid label").is_err());
    }
    snapshot.boot_id = target.boot_id.clone();
    snapshot.workspaces.swap(0, 6);
    assert!(
        target
            .request(&snapshot, WorkspaceAction::Close, "")
            .is_ok()
    );
    snapshot.workspaces.retain(|w| w.workspace_id != "w4");
    assert!(
        target
            .request(&snapshot, WorkspaceAction::Close, "")
            .is_err()
    );
    assert!(
        target
            .request(&snapshot, WorkspaceAction::Rename, "valid label")
            .is_ok()
    );
    snapshot.workspaces.retain(|w| w.workspace_id != "w3");
    assert!(
        target
            .request(&snapshot, WorkspaceAction::Rename, "valid label")
            .is_err()
    );
}

#[test]
fn rename_trims_unicode_whitespace_and_rejects_blank_labels() {
    let snapshot = sidebar::layout_tests::snapshot(7);
    let target = WorkspaceTarget::new(&snapshot, &snapshot.workspaces[3]);
    for text in ["", " \t\r\n", "\u{2003}\u{3000}"] {
        assert!(matches!(
            target.request(&snapshot, WorkspaceAction::Rename, text),
            Err(crate::Error::EmptyWorkspaceLabel)
        ));
    }
    assert_eq!(
        target
            .request(
                &snapshot,
                WorkspaceAction::Rename,
                " \u{3000}new label\u{2003} "
            )
            .unwrap(),
        (
            Method::WorkspaceRename,
            serde_json::json!({"workspace_id": "w3", "label": "new label"})
        )
    );
}
