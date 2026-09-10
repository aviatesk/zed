use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, FutureExt as _, Task, WeakEntity};

use futures::{FutureExt, future::Shared};
use itertools::Itertools as _;
use language::LanguageName;
use remote::{Interactive, RemoteClient};
use rpc::proto;
use settings::{Settings, SettingsLocation};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use task::{Shell, ShellBuilder, ShellKind, SpawnInTerminal};
use terminal::{
    Terminal, TerminalBuilder, TerminalMode, insert_zed_terminal_env,
    terminal_settings::TerminalSettings,
};
use util::{
    command::new_std_command, get_default_system_shell, get_system_shell, maybe, rel_path::RelPath,
};

use crate::{Project, ProjectPath};

pub struct Terminals {
    pub(crate) local_handles: Vec<WeakEntity<terminal::Terminal>>,
}

impl Project {
    pub fn active_entry_directory(&self, cx: &App) -> Option<PathBuf> {
        let entry_id = self.active_entry()?;
        let worktree = self.worktree_for_entry(entry_id, cx)?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_id(entry_id)?;

        let absolute_path = worktree.absolutize(entry.path.as_ref());
        if entry.is_dir() {
            Some(absolute_path)
        } else {
            absolute_path.parent().map(|p| p.to_path_buf())
        }
    }

    pub fn active_project_directory(&self, cx: &App) -> Option<Arc<Path>> {
        self.active_entry()
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .into_iter()
            .chain(self.worktrees(cx))
            .find_map(|tree| tree.read(cx).root_dir())
    }

    pub fn first_project_directory(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.worktrees(cx).next()?;
        let worktree = worktree.read(cx);
        if worktree.root_entry()?.is_dir() {
            Some(worktree.abs_path().to_path_buf())
        } else {
            None
        }
    }

    pub fn create_terminal_task(
        &mut self,
        spawn_task: SpawnInTerminal,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let is_via_remote = self.remote_client.is_some();

        let path: Option<Arc<Path>> = if let Some(cwd) = &spawn_task.cwd {
            if is_via_remote {
                Some(Arc::from(cwd.as_ref()))
            } else {
                let cwd = cwd.to_string_lossy();
                let tilde_substituted = shellexpand::tilde(&cwd);
                Some(Arc::from(Path::new(tilde_substituted.as_ref())))
            }
        } else {
            self.active_project_directory(cx)
        };

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let detect_venv = settings.detect_venv.as_option().is_some();

        let terminal_mode = TerminalMode::task(spawn_task.clone());

        let local_path = if is_via_remote { None } else { path.clone() };
        let remote_client = self.remote_client.clone();
        let shell = match &remote_client {
            Some(remote_client) => remote_client
                .read(cx)
                .shell()
                .unwrap_or_else(get_default_system_shell),
            None => get_system_shell(),
        };
        let path_style = self.path_style(cx);
        let shell_kind = ShellKind::new(&shell, path_style.is_windows());

        // Prepare a task for resolving the environment
        let env_task =
            self.resolve_directory_environment(&shell, path.clone(), remote_client.clone(), cx);

        // Scope the toolchain lookup to the worktree the terminal is being
        // spawned in. Previously this iterated the active editor's worktree
        // and then every visible worktree, so a Python toolchain persisted
        // for worktree A would leak into a terminal opened in worktree B and
        // inject (e.g.) `conda activate base` into a shell that has no
        // business with conda.
        let project_path_contexts: Vec<ProjectPath> = path
            .as_ref()
            .and_then(|p| self.find_worktree(p, cx))
            .map(|(worktree, relative_path)| ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: relative_path,
            })
            .into_iter()
            .collect();
        let toolchains = project_path_contexts
            .into_iter()
            .filter(|_| detect_venv)
            .map(|p| self.active_toolchain(p, LanguageName::new_static("Python"), cx))
            .collect::<Vec<_>>();
        let lang_registry = self.languages.clone();
        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env);

            let activation_script = maybe!(async {
                for toolchain in toolchains {
                    let Some(toolchain) = toolchain.await else {
                        continue;
                    };
                    let language = lang_registry
                        .language_for_name(&toolchain.language_name.0)
                        .await
                        .ok();
                    let lister = language?.toolchain_lister()?;
                    let future =
                        cx.update(|cx| lister.activation_script(&toolchain, shell_kind, cx));
                    return Some(future.await);
                }
                None
            })
            .await
            .unwrap_or_default();

            let builder = project
                .update(cx, move |_, cx| {
                    let format_to_run = |spawn_task: &SpawnInTerminal| {
                        format_task_for_activation(
                            spawn_task,
                            shell_kind,
                            &shell,
                            path_style.is_windows(),
                        )
                    };

                    let (shell, env) = {
                        let to_run =
                            (!activation_script.is_empty()).then(|| format_to_run(&spawn_task));
                        env.extend(spawn_task.env);
                        match remote_client {
                            Some(remote_client) => match activation_script.clone() {
                                activation_script if !activation_script.is_empty() => {
                                    let separator = shell_kind.sequential_commands_separator();
                                    let activation_script =
                                        activation_script.join(&format!("{separator} "));
                                    let to_run = to_run.expect("activation command was formatted");

                                    let arg = format!("{activation_script}{separator} {to_run}");
                                    let args = shell_kind.args_for_shell(true, arg);
                                    let shell = remote_client
                                        .read(cx)
                                        .shell()
                                        .unwrap_or_else(get_default_system_shell);

                                    create_remote_shell(
                                        Some((&shell, &args)),
                                        env,
                                        path,
                                        remote_client,
                                        cx,
                                    )?
                                }
                                _ => create_remote_shell(
                                    spawn_task
                                        .command
                                        .as_ref()
                                        .map(|command| (command, &spawn_task.args)),
                                    env,
                                    path,
                                    remote_client,
                                    cx,
                                )?,
                            },
                            None => match activation_script.clone() {
                                activation_script if !activation_script.is_empty() => {
                                    let separator = shell_kind.sequential_commands_separator();
                                    let activation_script =
                                        activation_script.join(&format!("{separator} "));
                                    let to_run = to_run.expect("activation command was formatted");

                                    let arg = format!("{activation_script}{separator} {to_run}");
                                    let args = shell_kind.args_for_shell(true, arg);

                                    (
                                        Shell::WithArguments {
                                            program: shell,
                                            args,
                                            title_override: None,
                                        },
                                        env,
                                    )
                                }
                                _ => (
                                    if let Some(program) = spawn_task.command {
                                        Shell::WithArguments {
                                            program,
                                            args: spawn_task.args,
                                            title_override: None,
                                        }
                                    } else {
                                        Shell::System
                                    },
                                    env,
                                ),
                            },
                        }
                    };
                    anyhow::Ok(TerminalBuilder::new(
                        local_path.map(|path| path.to_path_buf()),
                        terminal_mode,
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        Duration::from_millis(settings.path_hyperlink_timeout_ms),
                        is_via_remote,
                        cx.entity_id().as_u64(),
                        cx,
                        activation_script,
                        path_style,
                    ))
                })??
                .await?;
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn create_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.create_terminal_shell_internal(cwd, false, cx)
    }

    /// Creates a local terminal even if the project is remote.
    /// In remote projects: opens in Zed's launch directory (bypasses SSH).
    /// In local projects: opens in the project directory (same as regular terminals).
    pub fn create_local_terminal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let working_directory = if self.remote_client.is_some() {
            // Remote project: don't use remote paths, let shell use Zed's cwd
            None
        } else {
            // Local project: use project directory like normal terminals
            self.active_project_directory(cx).map(|p| p.to_path_buf())
        };
        self.create_terminal_shell_internal(working_directory, true, cx)
    }

    /// Internal method for creating terminal shells.
    /// If force_local is true, creates a local terminal even if the project has a remote client.
    /// This allows "breaking out" to a local shell in remote projects.
    fn create_terminal_shell_internal(
        &mut self,
        cwd: Option<PathBuf>,
        force_local: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path = cwd.map(|p| Arc::from(&*p));
        let is_via_remote = !force_local && self.remote_client.is_some();

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let detect_venv = settings.detect_venv.as_option().is_some();
        let local_path = if is_via_remote { None } else { path.clone() };

        // See create_terminal_task: scope the toolchain lookup to the
        // worktree the terminal is opened in, not the active editor's
        // worktree or other visible worktrees.
        let project_path_contexts: Vec<ProjectPath> = path
            .as_ref()
            .and_then(|p| self.find_worktree(p, cx))
            .map(|(worktree, relative_path)| ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: relative_path,
            })
            .into_iter()
            .collect();
        let toolchains = project_path_contexts
            .into_iter()
            .filter(|_| detect_venv)
            .map(|p| self.active_toolchain(p, LanguageName::new_static("Python"), cx))
            .collect::<Vec<_>>();
        let remote_client = if force_local {
            None
        } else {
            self.remote_client.clone()
        };
        let shell = match &remote_client {
            Some(remote_client) => remote_client
                .read(cx)
                .shell()
                .unwrap_or_else(get_default_system_shell),
            None => settings.shell.program(),
        };
        let env_shell = match &remote_client {
            Some(_) => shell.clone(),
            None => get_system_shell(),
        };

        let path_style = self.path_style(cx);

        // Prepare a task for resolving the environment
        let env_task =
            self.resolve_directory_environment(&env_shell, path.clone(), remote_client.clone(), cx);

        let shell_kind = ShellKind::new(&shell, path_style.is_windows());
        let remote_cli_info_request = remote_client.as_ref().and_then(|remote_client| {
            let remote_client = remote_client.read(cx);
            if remote_client.remote_platform().os.is_windows()
                || !matches!(
                    remote_client.connection_options(),
                    remote::RemoteConnectionOptions::Ssh(_)
                )
            {
                return None;
            }
            if !matches!(shell_kind, ShellKind::Posix | ShellKind::Fish) {
                log::debug!("Remote terminal CLI is not supported for shell {shell}");
                return None;
            }
            Some(
                remote_client
                    .proto_client()
                    .request(proto::GetRemoteCliInfo {}),
            )
        });

        let lang_registry = self.languages.clone();
        cx.spawn(async move |project, cx| {
            let (env, remote_cli_info) = futures::join!(env_task, async {
                let request = remote_cli_info_request?;
                // Older servers may not respond to this optional request.
                Some(
                    request
                        .with_timeout(Duration::from_secs(5), cx.background_executor())
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|response| response),
                )
            });
            let mut env = env.unwrap_or_default();
            env.extend(settings.env);

            let mut activation_script = maybe!(async {
                for toolchain in toolchains {
                    let Some(toolchain) = toolchain.await else {
                        continue;
                    };
                    let language = lang_registry
                        .language_for_name(&toolchain.language_name.0)
                        .await
                        .ok();
                    let lister = language?.toolchain_lister()?;
                    let future =
                        cx.update(|cx| lister.activation_script(&toolchain, shell_kind, cx));
                    return Some(future.await);
                }
                None
            })
            .await
            .unwrap_or_default();

            if let Some(remote_cli_info) = remote_cli_info {
                apply_remote_cli_info(
                    remote_cli_info,
                    shell_kind,
                    &mut env,
                    &mut activation_script,
                );
            }

            let builder = project
                .update(cx, move |_, cx| {
                    let (shell, env) = {
                        match remote_client {
                            Some(remote_client) => {
                                create_remote_shell(None, env, path, remote_client, cx)?
                            }
                            None => (settings.shell, env),
                        }
                    };
                    anyhow::Ok(TerminalBuilder::new(
                        local_path.map(|path| path.to_path_buf()),
                        TerminalMode::interactive(),
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        Duration::from_millis(settings.path_hyperlink_timeout_ms),
                        is_via_remote,
                        cx.entity_id().as_u64(),
                        cx,
                        activation_script,
                        path_style,
                    ))
                })??
                .await?;
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn clone_terminal(
        &mut self,
        terminal: &Entity<Terminal>,
        cx: &mut Context<'_, Project>,
        cwd: Option<PathBuf>,
    ) -> Task<Result<Entity<Terminal>>> {
        // We cannot clone the task's terminal, as it will effectively re-spawn the task, which might not be desirable.
        // For now, create a new shell instead.
        if terminal.read(cx).task().is_some() {
            return self.create_terminal_shell(cwd, cx);
        }
        let local_path = if self.is_via_remote_server() {
            None
        } else {
            cwd
        };

        let builder = terminal.read(cx).clone_builder(cx, local_path);
        cx.spawn(async |project, cx| {
            let terminal = builder.await?;
            project.update(cx, |project, cx| {
                let terminal_handle = cx.new(|cx| terminal.subscribe(cx));

                project
                    .terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn terminal_settings<'a>(
        &'a self,
        path: &'a Option<PathBuf>,
        cx: &'a App,
    ) -> &'a TerminalSettings {
        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        TerminalSettings::get(settings_location, cx)
    }

    pub fn exec_in_shell(
        &self,
        command: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<smol::process::Command>> {
        let path = self.first_project_directory(cx);
        let remote_client = self.remote_client.clone();
        let settings = self.terminal_settings(&path, cx).clone();
        let shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell())
            .map(Shell::Program)
            .unwrap_or(Shell::System);
        let is_windows = self.path_style(cx).is_windows();
        let builder = ShellBuilder::new(&shell, is_windows).non_interactive();
        let (command, args) = builder.build(Some(command), &Vec::new());

        let env_task = self.resolve_directory_environment(
            &shell.program(),
            path.as_ref().map(|p| Arc::from(&**p)),
            remote_client.clone(),
            cx,
        );

        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env);

            project.update(cx, move |_, cx| {
                match remote_client {
                    Some(remote_client) => {
                        let command_template = remote_client.read(cx).build_command(
                            Some(command),
                            &args,
                            &env,
                            None,
                            None,
                            Interactive::Yes,
                        )?;
                        let mut command = new_std_command(command_template.program);
                        command.args(command_template.args);
                        command.envs(command_template.env);
                        Ok(command)
                    }
                    None => {
                        let mut command = new_std_command(command);
                        command.args(args);
                        command.envs(env);
                        if let Some(path) = path {
                            command.current_dir(path);
                        }
                        Ok(command)
                    }
                }
                .map(|mut process| {
                    util::set_pre_exec_to_start_new_session(&mut process);
                    smol::process::Command::from(process)
                })
            })?
        })
    }

    pub fn local_terminal_handles(&self) -> &Vec<WeakEntity<terminal::Terminal>> {
        &self.terminals.local_handles
    }

    fn resolve_directory_environment(
        &self,
        shell: &str,
        path: Option<Arc<Path>>,
        remote_client: Option<Entity<RemoteClient>>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(path) = &path {
            let shell = Shell::Program(shell.to_string());
            self.environment
                .update(cx, |project_env, cx| match &remote_client {
                    Some(remote_client) => project_env.remote_directory_environment(
                        &shell,
                        path.clone(),
                        remote_client.clone(),
                        cx,
                    ),
                    None => project_env.local_directory_environment(&shell, path.clone(), cx),
                })
        } else {
            Task::ready(None).shared()
        }
    }
}

fn remote_cli_path_command(shell_kind: ShellKind, bin_path: &str) -> Option<String> {
    // This command is typed into a PTY, so shell quoting alone cannot protect
    // control characters. A colon cannot be represented within a PATH entry.
    if !bin_path.starts_with('/')
        || bin_path.contains(':')
        || bin_path.chars().any(char::is_control)
    {
        return None;
    }
    let bin_path = shell_kind.try_quote(bin_path)?;
    match shell_kind {
        ShellKind::Posix => Some(format!("export PATH={bin_path}:\"$PATH\"")),
        ShellKind::Fish => Some(format!("set -gx PATH {bin_path} $PATH")),
        _ => None,
    }
}

fn apply_remote_cli_info(
    remote_cli_info: Result<proto::RemoteCliInfo>,
    shell_kind: ShellKind,
    env: &mut HashMap<String, String>,
    activation_script: &mut Vec<String>,
) {
    let remote_cli_info = match remote_cli_info {
        Ok(remote_cli_info) => remote_cli_info,
        Err(error) => {
            log::warn!("Failed to get remote terminal CLI information: {error:#}");
            return;
        }
    };
    if remote_cli_info.socket_path.is_empty() || remote_cli_info.bin_path.is_empty() {
        log::debug!("Remote server does not support the terminal CLI");
        return;
    }
    if !remote_cli_info.socket_path.starts_with('/')
        || remote_cli_info.socket_path.chars().any(char::is_control)
    {
        log::warn!("Remote terminal CLI socket path is not supported");
        return;
    }
    let Some(command) = remote_cli_path_command(shell_kind, &remote_cli_info.bin_path) else {
        log::warn!("Remote terminal CLI shell or bin path is not supported");
        return;
    };

    env.insert("ZED_REMOTE_CLI_SOCKET".into(), remote_cli_info.socket_path);
    env.insert("ZED_REMOTE_CLI_BIN".into(), remote_cli_info.bin_path);
    // Login startup files and toolchain activation can overwrite PATH.
    activation_script.push(command);
}

fn create_remote_shell(
    spawn_command: Option<(&String, &Vec<String>)>,
    mut env: HashMap<String, String>,
    working_directory: Option<Arc<Path>>,
    remote_client: Entity<RemoteClient>,
    cx: &mut App,
) -> Result<(Shell, HashMap<String, String>)> {
    insert_zed_terminal_env(&mut env, &release_channel::AppVersion::global(cx));

    let (program, args) = match spawn_command {
        Some((program, args)) => (Some(program.clone()), args),
        None => (None, &Vec::new()),
    };

    let command = remote_client.read(cx).build_command(
        program,
        args.as_slice(),
        &env,
        working_directory.map(|path| path.display().to_string()),
        None,
        Interactive::Yes,
    )?;

    log::debug!("Connecting to a remote server: {:?}", command.program);
    let host = remote_client.read(cx).connection_options().display_name();

    Ok((
        Shell::WithArguments {
            program: command.program,
            args: command.args,
            title_override: Some(format!("{} — Terminal", host)),
        },
        command.env,
    ))
}

fn format_task_for_activation(
    spawn_task: &SpawnInTerminal,
    shell_kind: ShellKind,
    shell: &str,
    is_windows: bool,
) -> String {
    if let Some(command) = &spawn_task.command {
        let command = shell_kind.prepend_command_prefix(command);
        let command = shell_kind.try_quote_prefix_aware(&command);
        let args = spawn_task
            .args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                quote_prepared_task_arg_for_activation(
                    spawn_task, shell_kind, arg, index, is_windows,
                )
            });

        command.into_iter().chain(args).join(" ")
    } else {
        // todo: this breaks for remotes to windows
        format!("exec {shell} -l")
    }
}

fn quote_prepared_task_arg_for_activation<'a>(
    spawn_task: &SpawnInTerminal,
    shell_kind: ShellKind,
    arg: &'a str,
    index: usize,
    is_windows: bool,
) -> Option<Cow<'a, str>> {
    if spawn_task.shell.shell_kind(is_windows) == ShellKind::Cmd
        && index >= 2
        && spawn_task
            .args
            .get(index - 2)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/S"))
        && spawn_task
            .args
            .get(index - 1)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/C"))
    {
        // The /C argument is already a cmd command string from prepare_task_for_spawn.
        // Quoting it again for venv activation makes cmd see the quotes as literals.
        return quote_cmd_command_arg_for_outer_shell(arg, shell_kind).map(Cow::Owned);
    }

    shell_kind.try_quote(arg)
}

fn quote_cmd_command_arg_for_outer_shell(arg: &str, shell_kind: ShellKind) -> Option<String> {
    match shell_kind {
        ShellKind::PowerShell | ShellKind::Pwsh => Some(format!("'{}'", arg.replace('\'', "''"))),
        ShellKind::Cmd => Some(arg.to_string()),
        ShellKind::Posix
        | ShellKind::Csh
        | ShellKind::Tcsh
        | ShellKind::Fish
        | ShellKind::Nushell
        | ShellKind::Rc
        | ShellKind::Xonsh
        | ShellKind::Elvish => shell_kind.try_quote(arg).map(Cow::into_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn formats_remote_cli_path_commands() {
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh", "/bin/dash", "/bin/ksh"] {
            assert_eq!(
                remote_cli_path_command(ShellKind::new(shell, false), "/remote/cli bin"),
                Some("export PATH='/remote/cli bin':\"$PATH\"".to_string()),
            );
        }
        assert_eq!(
            remote_cli_path_command(ShellKind::Fish, "/remote/cli bin"),
            Some("set -gx PATH '/remote/cli bin' $PATH".to_string()),
        );
    }

    #[test]
    fn quotes_remote_cli_path_commands() {
        let bin_path = "/remote/$(touch marker); $HOME/bin";
        assert_eq!(
            remote_cli_path_command(ShellKind::Posix, bin_path),
            Some("export PATH='/remote/$(touch marker); $HOME/bin':\"$PATH\"".to_string()),
        );
        assert_eq!(
            remote_cli_path_command(ShellKind::Fish, bin_path),
            Some("set -gx PATH '/remote/$(touch marker); $HOME/bin' $PATH".to_string()),
        );

        let bin_path = r#"/remote/it's "quoted"; $(touch marker) `echo marker` \bin"#;
        let command = remote_cli_path_command(ShellKind::Posix, bin_path)
            .expect("POSIX path command should be supported");
        assert_eq!(
            ShellKind::Posix.split(&command),
            Some(vec!["export".to_string(), format!("PATH={bin_path}:$PATH")]),
        );
        let command = remote_cli_path_command(ShellKind::Fish, bin_path)
            .expect("fish path command should be supported");
        assert_eq!(
            ShellKind::Fish.split(&command),
            Some(vec![
                "set".to_string(),
                "-gx".to_string(),
                "PATH".to_string(),
                bin_path.to_string(),
                "$PATH".to_string(),
            ]),
        );
    }

    #[test]
    fn rejects_remote_cli_paths_unsafe_for_pty_input_or_path_entries() {
        for bin_path in [
            "",
            "relative/bin",
            "~/bin",
            "/remote/bin:other",
            "/remote/\0bin",
            "/remote/\nbin",
            "/remote/\rbin",
            "/remote/\tbin",
            "/remote/\x1bbin",
            "/remote/\x7fbin",
        ] {
            for shell_kind in [ShellKind::Posix, ShellKind::Fish] {
                assert_eq!(remote_cli_path_command(shell_kind, bin_path), None);
            }
        }
    }

    #[test]
    fn skips_unsupported_remote_cli_shells() {
        for shell_kind in [
            ShellKind::Csh,
            ShellKind::Tcsh,
            ShellKind::Rc,
            ShellKind::PowerShell,
            ShellKind::Pwsh,
            ShellKind::Nushell,
            ShellKind::Cmd,
            ShellKind::Xonsh,
            ShellKind::Elvish,
        ] {
            let mut env = HashMap::default();
            let mut activation_script = Vec::new();
            apply_remote_cli_info(
                Ok(proto::RemoteCliInfo {
                    socket_path: "/remote/cli.sock".to_string(),
                    bin_path: "/remote/bin".to_string(),
                }),
                shell_kind,
                &mut env,
                &mut activation_script,
            );
            assert!(env.is_empty());
            assert!(activation_script.is_empty());
        }
    }

    #[test]
    fn applies_remote_cli_info_after_environment_and_toolchain_activation() {
        for shell_kind in [ShellKind::Posix, ShellKind::Fish] {
            let mut env = HashMap::from_iter([
                ("PATH".to_string(), "/toolchain/bin:/usr/bin".to_string()),
                (
                    "ZED_REMOTE_CLI_SOCKET".to_string(),
                    "old socket".to_string(),
                ),
                ("ZED_REMOTE_CLI_BIN".to_string(), "old bin".to_string()),
            ]);
            let mut activation_script = vec!["source /toolchain/activate".to_string()];
            let socket_path = "/remote/session's socket.sock";
            let bin_path = "/remote/session's bin";
            apply_remote_cli_info(
                Ok(proto::RemoteCliInfo {
                    socket_path: socket_path.to_string(),
                    bin_path: bin_path.to_string(),
                }),
                shell_kind,
                &mut env,
                &mut activation_script,
            );
            assert_eq!(
                env,
                HashMap::from_iter([
                    ("PATH".to_string(), "/toolchain/bin:/usr/bin".to_string()),
                    ("ZED_REMOTE_CLI_SOCKET".to_string(), socket_path.to_string()),
                    ("ZED_REMOTE_CLI_BIN".to_string(), bin_path.to_string()),
                ]),
            );
            assert_eq!(
                activation_script,
                vec![
                    "source /toolchain/activate".to_string(),
                    remote_cli_path_command(shell_kind, bin_path)
                        .expect("path command should be supported"),
                ],
            );
        }
    }

    #[test]
    fn unavailable_remote_cli_info_preserves_terminal_preparation() {
        let mut responses = vec![Err(anyhow::anyhow!("request failed"))];
        for (socket_path, bin_path) in [
            ("", ""),
            ("", "/remote/bin"),
            ("/remote/cli.sock", ""),
            ("relative.sock", "/remote/bin"),
            ("/remote/\0cli.sock", "/remote/bin"),
            ("/remote/cli.sock", "/remote/\nbin"),
        ] {
            responses.push(Ok(proto::RemoteCliInfo {
                socket_path: socket_path.to_string(),
                bin_path: bin_path.to_string(),
            }));
        }
        for response in responses {
            let original_env = HashMap::from_iter([("PATH".to_string(), "/usr/bin".to_string())]);
            let original_activation_script = vec!["source /toolchain/activate".to_string()];
            let mut env = original_env.clone();
            let mut activation_script = original_activation_script.clone();
            apply_remote_cli_info(response, ShellKind::Posix, &mut env, &mut activation_script);
            assert_eq!(env, original_env);
            assert_eq!(activation_script, original_activation_script);
        }
    }

    fn prepared_cmd_task(command_arg: &str) -> SpawnInTerminal {
        SpawnInTerminal {
            command: Some("cmd.exe".to_string()),
            args: vec!["/S".to_string(), "/C".to_string(), command_arg.to_string()],
            shell: Shell::Program("cmd.exe".to_string()),
            ..SpawnInTerminal::default()
        }
    }

    #[test]
    fn formats_prepared_cmd_task_for_powershell_activation() {
        let task = prepared_cmd_task("\"echo Hi there\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /S /C '\"echo Hi there\"'"
        );
    }

    #[test]
    fn formats_prepared_cmd_task_for_cmd_activation() {
        let task = prepared_cmd_task("\"echo Hi there\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::Cmd, "cmd.exe", true),
            "cmd.exe /S /C \"echo Hi there\""
        );
    }

    #[test]
    fn formats_prepared_cmd_task_with_shell_args_for_activation() {
        let task = SpawnInTerminal {
            command: Some("cmd.exe".to_string()),
            args: vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                "\"echo Hi there\"".to_string(),
            ],
            shell: Shell::WithArguments {
                program: "cmd.exe".to_string(),
                args: vec!["/D".to_string()],
                title_override: None,
            },
            ..SpawnInTerminal::default()
        };

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /D /S /C '\"echo Hi there\"'"
        );
    }

    #[test]
    fn formats_prepared_cmd_task_with_single_quote_for_powershell_activation() {
        let task = prepared_cmd_task("\"echo It's fine\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /S /C '\"echo It''s fine\"'"
        );
    }

    #[test]
    fn formats_non_cmd_task_for_activation() {
        let task = SpawnInTerminal {
            command: Some("cargo".to_string()),
            args: vec!["test".to_string(), "some test".to_string()],
            shell: Shell::System,
            ..SpawnInTerminal::default()
        };

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cargo test 'some test'"
        );
    }
}
