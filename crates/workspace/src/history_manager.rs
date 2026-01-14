use std::path::PathBuf;

use gpui::{AppContext, Entity, Global, JumpListEntry, MenuItem, RemoteConnectionInfo};
use remote::RemoteConnectionOptions;
use smallvec::SmallVec;
use ui::App;
use util::{ResultExt, paths::PathExt};

use crate::{
    NewWindow, SerializedWorkspaceLocation, WORKSPACE_DB, WorkspaceId, path_list::PathList,
};

pub fn init(cx: &mut App) {
    let manager = cx.new(|_| HistoryManager::new());
    HistoryManager::set_global(manager.clone(), cx);
    HistoryManager::init(manager, cx);
}

pub struct HistoryManager {
    /// The history of workspaces that have been opened in the past, in reverse order.
    /// The most recent workspace is at the end of the vector.
    history: Vec<HistoryManagerEntry>,
}

#[derive(Debug)]
pub struct HistoryManagerEntry {
    pub id: WorkspaceId,
    pub location: SerializedWorkspaceLocation,
    pub paths: SmallVec<[PathBuf; 2]>,
}

struct GlobalHistoryManager(Entity<HistoryManager>);

impl Global for GlobalHistoryManager {}

impl HistoryManager {
    fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }

    fn init(this: Entity<HistoryManager>, cx: &App) {
        cx.spawn(async move |cx| {
            let recent_folders = WORKSPACE_DB
                .recent_workspaces_on_disk()
                .await
                .unwrap_or_default()
                .into_iter()
                .rev()
                .map(|(id, location, paths)| HistoryManagerEntry::new(id, location, &paths))
                .collect::<Vec<_>>();
            this.update(cx, |this, cx| {
                this.history = recent_folders;
                this.update_jump_list(cx);
            })
        })
        .detach();
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalHistoryManager>()
            .map(|model| model.0.clone())
    }

    fn set_global(history_manager: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalHistoryManager(history_manager));
    }

    pub fn update_history(&mut self, id: WorkspaceId, entry: HistoryManagerEntry, cx: &App) {
        if let Some(pos) = self.history.iter().position(|e| e.id == id) {
            self.history.remove(pos);
        }
        self.history.push(entry);
        self.update_jump_list(cx);
    }

    pub fn delete_history(&mut self, id: WorkspaceId, cx: &App) {
        let Some(pos) = self.history.iter().position(|e| e.id == id) else {
            return;
        };
        self.history.remove(pos);
        self.update_jump_list(cx);
    }

    fn update_jump_list(&mut self, cx: &App) {
        let menus = vec![MenuItem::action("New Window", NewWindow)];
        let entries = self
            .history
            .iter()
            .rev()
            .map(|entry| entry.to_jump_list_entry())
            .collect::<Vec<_>>();
        let user_removed = cx.update_jump_list(menus, entries);
        self.remove_user_removed_workspaces(user_removed, cx);
    }

    pub fn remove_user_removed_workspaces(
        &mut self,
        user_removed: Vec<JumpListEntry>,
        cx: &App,
    ) {
        if user_removed.is_empty() {
            return;
        }
        let mut deleted_ids = Vec::new();
        for idx in (0..self.history.len()).rev() {
            if let Some(entry) = self.history.get(idx) {
                let jump_entry = entry.to_jump_list_entry();
                if user_removed.contains(&jump_entry) {
                    deleted_ids.push(entry.id);
                    self.history.remove(idx);
                }
            }
        }
        cx.spawn(async move |_| {
            for id in deleted_ids.iter() {
                WORKSPACE_DB.delete_workspace_by_id(*id).await.log_err();
            }
        })
        .detach();
    }
}

impl HistoryManagerEntry {
    pub fn new(id: WorkspaceId, location: SerializedWorkspaceLocation, paths: &PathList) -> Self {
        let paths = paths
            .ordered_paths()
            .map(|path| path.compact())
            .collect::<SmallVec<[PathBuf; 2]>>();
        Self { id, location, paths }
    }

    pub fn to_jump_list_entry(&self) -> JumpListEntry {
        match &self.location {
            SerializedWorkspaceLocation::Local => JumpListEntry::Local(self.paths.clone()),
            SerializedWorkspaceLocation::Remote(connection) => {
                let connection_info = match connection {
                    RemoteConnectionOptions::Ssh(opts) => RemoteConnectionInfo::Ssh {
                        username: opts.username.clone(),
                        host: opts.host.to_string(),
                        port: opts.port,
                    },
                    RemoteConnectionOptions::Wsl(opts) => RemoteConnectionInfo::Wsl {
                        distro: opts.distro_name.clone(),
                        user: opts.user.clone(),
                    },
                    RemoteConnectionOptions::Docker(opts) => RemoteConnectionInfo::Docker {
                        name: opts.name.clone(),
                    },
                    #[cfg(any(test, feature = "test-support"))]
                    RemoteConnectionOptions::Mock(opts) => RemoteConnectionInfo::Ssh {
                        username: None,
                        host: format!("mock-{}", opts.id),
                        port: None,
                    },
                };
                JumpListEntry::Remote {
                    connection: connection_info,
                    paths: self.paths.clone(),
                }
            }
        }
    }
}
