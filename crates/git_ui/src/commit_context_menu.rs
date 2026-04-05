use crate::commit_view::CommitView;
use anyhow::Context as _;
use git::Oid;
use gpui::{Action, ClipboardItem, Entity, FocusHandle, SharedString, WeakEntity, Window, actions};
use project::{GIT_COMMAND_TASK_TAG, git_store::Repository};

use task::{TaskContext, TaskVariables, VariableName};
use ui::{Color, ContextMenu, ContextMenuEntry, IconName, IconPosition, prelude::*};
use workspace::{Workspace, notifications::NotifyTaskExt};

actions!(
    git_graph,
    [
        /// Copies the SHA of the selected commit to the clipboard.
        CopyCommitSha,
        /// Copies a tag from the selected commit to the clipboard.
        CopyCommitTag,
        /// Opens the commit view for the selected commit.
        OpenCommitView,
    ]
);

const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitGraph,
    GitPanel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommitContextMenuRef {
    Branch(SharedString),
    Tag(SharedString),
}

impl CommitContextMenuRef {
    fn name(&self) -> &SharedString {
        match self {
            Self::Branch(name) | Self::Tag(name) => name,
        }
    }
}

pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    reference: Option<CommitContextMenuRef>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let sha = commit.sha;
    let sha_short = sha.display_short();
    let ref_name = reference.as_ref().map(|reference| reference.name().clone());
    let branch_ref = reference.and_then(|reference| match reference {
        CommitContextMenuRef::Branch(branch_ref) => Some(branch_ref),
        CommitContextMenuRef::Tag(_) => None,
    });
    let is_branch_ref = branch_ref.is_some();
    let git_tasks = git_context_menu_tasks(
        git_task_context(&repository, sha, ref_name.as_deref(), cx),
        &workspace,
        cx,
    );
    let header = match &ref_name {
        Some(ref_name) => format!("Ref {ref_name}"),
        None => format!("Commit {sha_short}"),
    };

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        context_menu
            .context(focus_handle)
            .header(header)
            .when_some(branch_ref, |menu, branch_ref| {
                let repository = repository.clone();
                let workspace = workspace.clone();
                menu.entry("View Branch Changes", None, move |window, cx| {
                    open_branch_changes(
                        sha,
                        branch_ref.clone(),
                        repository.clone(),
                        workspace.clone(),
                        window,
                        cx,
                    );
                })
            })
            .when(!is_branch_ref, |menu| {
                menu.entry("View Diff", Some(OpenCommitView.boxed_clone()), {
                    let repository = repository.clone();
                    let workspace = workspace.clone();
                    move |window, cx| {
                        let Some(repository) = repository.clone() else {
                            return;
                        };
                        CommitView::open(
                            sha.to_string(),
                            repository,
                            workspace.clone(),
                            None,
                            None,
                            window,
                            cx,
                        );
                    }
                })
            })
            .entry(
                "Copy SHA",
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry("Copy Ref Name", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = "Copy Tag";

                    match tag_names.as_slice() {
                        [] => menu.item(
                            ContextMenuEntry::new(copy_tag_label)
                                .action(CopyCommitTag.boxed_clone())
                                .disabled(true),
                        ),
                        [tag_name] => {
                            let tag_name = tag_name.clone();
                            let label = format!("{copy_tag_label}: {tag_name}");
                            menu.entry(
                                label,
                                Some(CopyCommitTag.boxed_clone()),
                                move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name.to_string(),
                                    ));
                                },
                            )
                        }
                        _ => menu.submenu(copy_tag_label, move |menu, _window, _cx| {
                            let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());

                            for tag_name in tag_names.clone() {
                                let tag_name_to_copy = tag_name.clone();
                                menu = menu.entry(tag_name, None, move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name_to_copy.to_string(),
                                    ));
                                });
                            }
                            menu
                        }),
                    }
                })
            })
            .when(source == CommitContextMenuSource::GitPanel, |menu| {
                menu.entry("Show in Git Graph", None, move |window, cx| {
                    window.dispatch_action(
                        Box::new(crate::git_graph::OpenAtCommit {
                            sha: sha.to_string(),
                        }),
                        cx,
                    );
                })
            })
            .map(|mut menu| {
                menu = menu.separator().header("Custom Commands");

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new("Learn More")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .icon_position(IconPosition::End)
                            .handler(|_window, cx| {
                                let docs_url =
                                    release_channel::docs_url(CUSTOM_GIT_COMMANDS_DOCS_SLUG, cx);
                                cx.open_url(&docs_url);
                            }),
                    );
                }

                for (task_source_kind, resolved_task) in git_tasks {
                    let label = resolved_task.display_label().to_string();
                    let workspace = workspace.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.schedule_resolved_task(
                                    task_source_kind.clone(),
                                    resolved_task.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    });
                }

                menu
            })
    })
}

fn open_branch_changes(
    head_sha: Oid,
    branch_ref: SharedString,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(repository) = repository else {
        return;
    };
    let default_branch = repository.update(cx, |repository, _| repository.default_branch(true));
    let workspace_for_errors = workspace.clone();

    window
        .spawn(cx, async move |cx| {
            let base_ref = default_branch?
                .await??
                .context("Could not determine the default branch")?;
            let open_task = cx.update(|window, cx| {
                CommitView::open_branch_diff(
                    base_ref,
                    branch_ref.clone(),
                    branch_ref,
                    head_sha.to_string(),
                    repository,
                    workspace,
                    window,
                    cx,
                )
            })?;
            open_task.await
        })
        .detach_and_notify_err(workspace_for_errors, window, cx);
}

fn git_task_context(
    repository: &Option<WeakEntity<Repository>>,
    commit_sha: git::Oid,
    ref_name: Option<&str>,
    cx: &App,
) -> Option<TaskContext> {
    let repository_path = repository
        .as_ref()?
        .upgrade()?
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string);
    let mut task_variables = TaskVariables::from_iter([
        (VariableName::GitSha, commit_sha.to_string()),
        (VariableName::GitShaShort, commit_sha.display_short()),
        (
            VariableName::GitRepositoryPath,
            repository_path.to_string_lossy().into_owned(),
        ),
    ]);

    if let Some(repository_name) = repository_name {
        task_variables.insert(VariableName::GitRepositoryName, repository_name);
    }
    if let Some(ref_name) = ref_name {
        task_variables.insert(VariableName::GitRef, ref_name.to_string());
    }

    Some(TaskContext {
        cwd: Some(repository_path),
        task_variables,
        ..TaskContext::default()
    })
}

fn git_context_menu_tasks(
    task_context: Option<TaskContext>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
    let Some(task_context) = task_context else {
        return Vec::new();
    };
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let project = workspace.read(cx).project().clone();
    let task_inventory = project.read_with(cx, |project, cx| {
        project.task_store().read(cx).task_inventory().cloned()
    });
    let Some(task_inventory) = task_inventory else {
        return Vec::new();
    };

    task_inventory
        .read(cx)
        .resolve_global_tasks_with_tag(GIT_COMMAND_TASK_TAG, &task_context)
}
