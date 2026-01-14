use std::path::PathBuf;

use itertools::Itertools;
use smallvec::SmallVec;
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        Globalization::u_strlen,
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, StructuredStorage::PROPVARIANT},
        UI::{
            Controls::INFOTIPSIZE,
            Shell::{
                Common::{IObjectArray, IObjectCollection},
                DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
                PropertiesSystem::IPropertyStore,
                ShellLink,
            },
        },
    },
    core::{GUID, HSTRING, Interface},
};

use crate::{Action, JumpListEntry, MenuItem, RemoteConnectionInfo};

pub(crate) struct JumpList {
    pub(crate) dock_menus: Vec<DockMenuItem>,
    pub(crate) recent_workspaces: Vec<JumpListEntry>,
}

impl JumpList {
    pub(crate) fn new() -> Self {
        Self {
            dock_menus: Vec::new(),
            recent_workspaces: Vec::new(),
        }
    }
}

pub(crate) struct DockMenuItem {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) action: Box<dyn Action>,
}

impl DockMenuItem {
    pub(crate) fn new(item: MenuItem) -> anyhow::Result<Self> {
        match item {
            MenuItem::Action { name, action, .. } => Ok(Self {
                name: name.clone().into(),
                description: if name == "New Window" {
                    "Opens a new window".to_string()
                } else {
                    name.into()
                },
                action,
            }),
            _ => anyhow::bail!("Only `MenuItem::Action` is supported for dock menu on Windows."),
        }
    }
}

// This code is based on the example from Microsoft:
// https://github.com/microsoft/Windows-classic-samples/blob/main/Samples/Win7Samples/winui/shell/appshellintegration/RecipePropertyHandler/RecipePropertyHandler.cpp
pub(crate) fn update_jump_list(jump_list: &JumpList) -> anyhow::Result<Vec<JumpListEntry>> {
    let (list, removed) = create_destination_list()?;
    add_recent_folders(&list, &jump_list.recent_workspaces, &removed)?;
    add_dock_menu(&list, &jump_list.dock_menus)?;
    unsafe { list.CommitList() }?;
    Ok(removed)
}

// Copied from:
// https://github.com/microsoft/windows-rs/blob/0fc3c2e5a13d4316d242bdeb0a52af611eba8bd4/crates/libs/windows/src/Windows/Win32/Storage/EnhancedStorage/mod.rs#L1881
const PKEY_TITLE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9),
    pid: 2,
};

fn create_destination_list() -> anyhow::Result<(ICustomDestinationList, Vec<JumpListEntry>)> {
    let list: ICustomDestinationList =
        unsafe { CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER) }?;

    let mut slots = 0;
    let user_removed: IObjectArray = unsafe { list.BeginList(&mut slots) }?;

    let count = unsafe { user_removed.GetCount() }?;
    if count == 0 {
        return Ok((list, Vec::new()));
    }

    let mut removed = Vec::with_capacity(count as usize);
    for i in 0..count {
        let shell_link: IShellLinkW = unsafe { user_removed.GetAt(i)? };
        let description = {
            // INFOTIPSIZE is the maximum size of the buffer
            // see https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ishelllinkw-getdescription
            let mut buffer = [0u16; INFOTIPSIZE as usize];
            unsafe { shell_link.GetDescription(&mut buffer)? };
            let len = unsafe { u_strlen(buffer.as_ptr()) };
            String::from_utf16_lossy(&buffer[..len as usize])
        };
        removed.push(parse_description_to_entry(&description));
    }

    Ok((list, removed))
}

fn parse_description_to_entry(description: &str) -> JumpListEntry {
    // Try parsing as URL first (new format)
    if let Ok(url) = url::Url::parse(description) {
        if let Some(entry) = parse_url_to_entry(&url) {
            return entry;
        }
    }

    // Legacy format: "remote:<scheme>\n<connection>\n<paths...>"
    if let Some(rest) = description.strip_prefix("remote:") {
        if let Some(entry) = parse_legacy_remote_format(rest) {
            return entry;
        }
    }

    // Fallback: local entry (paths joined by newlines)
    JumpListEntry::Local(description.lines().map(PathBuf::from).collect())
}

fn parse_url_to_entry(url: &url::Url) -> Option<JumpListEntry> {
    let paths = collect_paths_from_url(url);

    let connection = match url.scheme() {
        "ssh" => RemoteConnectionInfo::Ssh {
            username: if url.username().is_empty() {
                None
            } else {
                Some(url.username().to_string())
            },
            host: url.host_str()?.to_string(),
            port: url.port(),
        },
        "wsl" => RemoteConnectionInfo::Wsl {
            distro: url.host_str()?.to_string(),
            user: url
                .query_pairs()
                .find(|(k, _)| k == "user")
                .map(|(_, v)| v.to_string()),
        },
        "docker" => RemoteConnectionInfo::Docker {
            name: url.host_str()?.to_string(),
        },
        _ => return None,
    };

    Some(JumpListEntry::Remote { connection, paths })
}

fn parse_legacy_remote_format(rest: &str) -> Option<JumpListEntry> {
    let mut lines = rest.lines();
    let scheme = lines.next()?;
    let connection_line = lines.next()?;
    let paths: SmallVec<[PathBuf; 2]> = lines.map(PathBuf::from).collect();

    let connection = match scheme {
        "ssh" => {
            // Parse "user@host:port" or "host:port" or "host"
            let (username, host_port) = connection_line
                .split_once('@')
                .map(|(u, h)| (Some(u.to_string()), h))
                .unwrap_or((None, connection_line));
            let (host, port) = host_port
                .rsplit_once(':')
                .map(|(h, p)| (h.to_string(), p.parse().ok()))
                .unwrap_or_else(|| (host_port.to_string(), None));
            RemoteConnectionInfo::Ssh {
                username,
                host,
                port,
            }
        }
        "wsl" => RemoteConnectionInfo::Wsl {
            distro: connection_line.to_string(),
            user: None, // Legacy format didn't support user
        },
        "docker" => RemoteConnectionInfo::Docker {
            name: connection_line.to_string(),
        },
        _ => return None,
    };

    Some(JumpListEntry::Remote { connection, paths })
}

fn collect_paths_from_url(url: &url::Url) -> SmallVec<[PathBuf; 2]> {
    let mut paths = SmallVec::new();
    paths.push(PathBuf::from(url.path()));
    for (key, value) in url.query_pairs() {
        if key == "path" {
            paths.push(PathBuf::from(value.as_ref()));
        }
    }
    paths
}

fn add_dock_menu(list: &ICustomDestinationList, dock_menus: &[DockMenuItem]) -> anyhow::Result<()> {
    unsafe {
        let tasks: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        for (idx, dock_menu) in dock_menus.iter().enumerate() {
            let argument = HSTRING::from(format!("--dock-action {}", idx));
            let description = HSTRING::from(dock_menu.description.as_str());
            let display = dock_menu.name.as_str();
            let task = create_shell_link(argument, description, None, display)?;
            tasks.AddObject(&task)?;
        }
        list.AddUserTasks(&tasks)?;
        Ok(())
    }
}

fn add_recent_folders(
    list: &ICustomDestinationList,
    entries: &[JumpListEntry],
    removed: &[JumpListEntry],
) -> anyhow::Result<()> {
    unsafe {
        let tasks: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;

        for entry in entries.iter().filter(|e| !removed.contains(e)) {
            let (argument, description, display) = match entry {
                JumpListEntry::Local(paths) => {
                    let argument = paths
                        .iter()
                        .map(|path| format!("\"{}\"", path.display()))
                        .join(" ");
                    let description = paths
                        .iter()
                        .map(|path| path.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let display = paths
                        .iter()
                        .map(|p| {
                            p.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_else(|| p.to_string_lossy())
                        })
                        .join(", ");
                    (argument, description, display)
                }
                JumpListEntry::Remote { connection, paths } => {
                    let url = format_remote_url(connection, paths);
                    let argument = format!("\"{}\"", url);
                    // Use URL directly as description - enables robust parsing via url::Url
                    let description = url.clone();

                    // Display: "project_name [ssh: host]"
                    let folder_names = paths
                        .iter()
                        .map(|p| {
                            p.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_else(|| p.to_string_lossy())
                        })
                        .join(", ");
                    let display = format!(
                        "{} [{}: {}]",
                        folder_names,
                        connection.scheme(),
                        connection.display_identifier()
                    );

                    (argument, description, display)
                }
            };

            // simulate folder icon
            // https://github.com/microsoft/vscode/blob/7a5dc239516a8953105da34f84bae152421a8886/src/vs/platform/workspaces/electron-main/workspacesHistoryMainService.ts#L380
            let icon = HSTRING::from("explorer.exe");

            tasks.AddObject(&create_shell_link(
                HSTRING::from(argument),
                HSTRING::from(description),
                Some(icon),
                &display,
            )?)?;
        }

        if tasks.GetCount().unwrap_or(0) > 0 {
            list.AppendCategory(&HSTRING::from("Recent Folders"), &tasks)?;
        }
        Ok(())
    }
}

/// Formats a proper URL for opening the remote workspace.
/// Uses standard URL schemes: ssh://, wsl://, docker://
fn format_remote_url(connection: &RemoteConnectionInfo, paths: &SmallVec<[PathBuf; 2]>) -> String {
    let first_path = paths
        .first()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut url = match connection {
        RemoteConnectionInfo::Ssh {
            username,
            host,
            port,
        } => {
            let mut url = url::Url::parse("ssh://placeholder").expect("valid base URL");
            url.set_host(Some(host)).ok();
            if let Some(user) = username {
                url.set_username(user).ok();
            }
            if let Some(p) = port {
                url.set_port(Some(*p)).ok();
            }
            url.set_path(&first_path);
            url
        }
        RemoteConnectionInfo::Wsl { distro, user } => {
            let mut url = url::Url::parse("wsl://placeholder").expect("valid base URL");
            url.set_host(Some(distro)).ok();
            url.set_path(&first_path);
            if let Some(u) = user {
                url.query_pairs_mut().append_pair("user", u);
            }
            url
        }
        RemoteConnectionInfo::Docker { name } => {
            let mut url = url::Url::parse("docker://placeholder").expect("valid base URL");
            url.set_host(Some(name)).ok();
            url.set_path(&first_path);
            url
        }
    };

    // Add additional paths as query params
    for path in paths.iter().skip(1) {
        url.query_pairs_mut()
            .append_pair("path", &path.to_string_lossy());
    }

    url.to_string()
}

fn create_shell_link(
    argument: HSTRING,
    description: HSTRING,
    icon: Option<HSTRING>,
    display: &str,
) -> anyhow::Result<IShellLinkW> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let exe_path = HSTRING::from(std::env::current_exe()?.as_os_str());
        link.SetPath(&exe_path)?;
        link.SetArguments(&argument)?;
        link.SetDescription(&description)?;
        if let Some(icon) = icon {
            link.SetIconLocation(&icon, 0)?;
        }
        let store: IPropertyStore = link.cast()?;
        let title = PROPVARIANT::from(display);
        store.SetValue(&PKEY_TITLE, &title)?;
        store.Commit()?;

        Ok(link)
    }
}
