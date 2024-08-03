use futures::future::{join_all, try_join_all};
use indexmap::IndexMap;
use log::info;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commands::async_command::{async_command, AsyncCallResult, With};
use serde::{Deserialize, Serialize};
use tauri::{Manager, State, Window};
use tauri_plugin_dialog::DialogExt;
use tokio::fs::write;
use url::Url;
use vrc_get_vpm::environment::{
    add_remote_repo, clear_package_cache, AddUserPackageResult, Settings, UserPackageCollection,
};
use vrc_get_vpm::io::{DefaultEnvironmentIo, IoTrait};
use vrc_get_vpm::repositories_file::RepositoriesFile;
use vrc_get_vpm::repository::RemoteRepository;
use vrc_get_vpm::{HttpClient, PackageInfo, VersionSelector};

use crate::commands::prelude::*;
use crate::specta::IndexMapV2;

#[derive(Serialize, specta::Type)]
pub struct TauriPackage {
    env_version: u32,
    index: usize,

    #[serde(flatten)]
    base: TauriBasePackageInfo,

    source: TauriPackageSource,
}

#[derive(Serialize, specta::Type)]
enum TauriPackageSource {
    LocalUser,
    Remote { id: String, display_name: String },
}

impl TauriPackage {
    fn new(env_version: u32, index: usize, package: &PackageInfo) -> Self {
        let source = if let Some(repo) = package.repo() {
            let id = repo.id().or(repo.url().map(|x| x.as_str())).unwrap();
            TauriPackageSource::Remote {
                id: id.to_string(),
                display_name: repo.name().unwrap_or(id).to_string(),
            }
        } else {
            TauriPackageSource::LocalUser
        };

        Self {
            env_version,
            index,
            base: TauriBasePackageInfo::new(package.package_json()),
            source,
        }
    }
}

#[tauri::command]
#[specta::specta]
pub async fn environment_refetch_packages(
    packages: State<'_, PackagesState>,
    settings: State<'_, SettingsState>,
    io: State<'_, DefaultEnvironmentIo>,
    http: State<'_, reqwest::Client>,
) -> Result<(), RustError> {
    let settings = settings.load(io.inner()).await?;
    packages
        .load_force(&settings, io.inner(), http.inner())
        .await?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn environment_packages(
    packages: State<'_, PackagesState>,
    settings: State<'_, SettingsState>,
    io: State<'_, DefaultEnvironmentIo>,
    http: State<'_, reqwest::Client>,
) -> Result<Vec<TauriPackage>, RustError> {
    let settings = settings.load(io.inner()).await?;
    let packages = packages.load(&settings, io.inner(), http.inner()).await?;

    let version = packages.version();

    Ok(packages
        .packages()
        .enumerate()
        .map(|(index, value)| TauriPackage::new(version, index, value))
        .collect::<Vec<_>>())
}

#[derive(Serialize, specta::Type)]
struct TauriUserRepository {
    id: String,
    url: Option<String>,
    display_name: String,
}

#[derive(Serialize, specta::Type)]
pub struct TauriRepositoriesInfo {
    user_repositories: Vec<TauriUserRepository>,
    hidden_user_repositories: Vec<String>,
    hide_local_user_packages: bool,
    show_prerelease_packages: bool,
}

#[tauri::command]
#[specta::specta]
pub async fn environment_repositories_info(
    settings: State<'_, SettingsState>,
    config: State<'_, GuiConfigState>,
    io: State<'_, DefaultEnvironmentIo>,
) -> Result<TauriRepositoriesInfo, RustError> {
    let config = config.get();
    let hidden_user_repositories = config.gui_hidden_repositories.iter().cloned().collect();
    let hide_local_user_packages = config.hide_local_user_packages;
    drop(config);

    let settings = settings.load(io.inner()).await?;
    let user_repositories = settings
        .get_user_repos()
        .iter()
        .map(|x| {
            let id = x.id().or(x.url().map(Url::as_str)).unwrap();
            TauriUserRepository {
                id: id.to_string(),
                url: x.url().map(|x| x.to_string()),
                display_name: x.name().unwrap_or(id).to_string(),
            }
        })
        .collect();
    let show_prerelease_packages = settings.show_prerelease_packages();

    Ok(TauriRepositoriesInfo {
        user_repositories,
        hidden_user_repositories,
        hide_local_user_packages,
        show_prerelease_packages,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn environment_hide_repository(
    config: State<'_, GuiConfigState>,
    repository: String,
) -> Result<(), RustError> {
    let mut config = config.load_mut().await?;
    config.gui_hidden_repositories.insert(repository);
    config.save().await?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn environment_show_repository(
    config: State<'_, GuiConfigState>,
    repository: String,
) -> Result<(), RustError> {
    let mut config = config.load_mut().await?;
    config.gui_hidden_repositories.shift_remove(&repository);
    config.save().await?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn environment_set_hide_local_user_packages(
    config: State<'_, GuiConfigState>,
    value: bool,
) -> Result<(), RustError> {
    let mut config = config.load_mut().await?;
    config.hide_local_user_packages = value;
    config.save().await?;
    Ok(())
}

#[derive(Serialize, specta::Type, Clone)]
pub struct TauriRemoteRepositoryInfo {
    display_name: String,
    id: String,
    url: String,
    packages: Vec<TauriBasePackageInfo>,
}

#[derive(Serialize, specta::Type, Clone)]
#[serde(tag = "type")]
pub enum TauriDownloadRepository {
    BadUrl,
    Duplicated,
    DownloadError { message: String },
    Success { value: TauriRemoteRepositoryInfo },
}

// workaround IndexMap v2 is not implemented in specta

#[tauri::command]
#[specta::specta]
pub async fn environment_download_repository(
    settings: State<'_, SettingsState>,
    io: State<'_, DefaultEnvironmentIo>,
    http: State<'_, reqwest::Client>,
    url: String,
    headers: IndexMapV2<Box<str>, Box<str>>,
) -> Result<TauriDownloadRepository, RustError> {
    let url: Url = match url.parse() {
        Err(_) => {
            return Ok(TauriDownloadRepository::BadUrl);
        }
        Ok(url) => url,
    };

    {
        let settings = settings.load(io.inner()).await?;
        let user_repo_urls = user_repo_urls(&settings);
        let user_repo_ids = user_repo_ids(&settings);

        download_one_repository(
            http.inner(),
            &url,
            &headers.0,
            &user_repo_urls,
            &user_repo_ids,
        )
        .await
    }
}

fn user_repo_urls(settings: &Settings) -> HashSet<String> {
    let mut user_repo_urls = settings
        .get_user_repos()
        .iter()
        .flat_map(|x| x.url())
        .map(|x| x.to_string())
        .collect::<HashSet<_>>();

    if !settings.ignore_curated_repository() {
        // should we check more urls?
        user_repo_urls.insert("https://packages.vrchat.com/curated?download".to_owned());
    }

    if !settings.ignore_official_repository() {
        user_repo_urls.insert("https://packages.vrchat.com/official?download".to_owned());
    }

    user_repo_urls
}

fn user_repo_ids(settings: &Settings) -> HashSet<String> {
    let mut user_repo_ids = settings
        .get_user_repos()
        .iter()
        .flat_map(|x| x.id())
        .map(|x| x.to_string())
        .collect::<HashSet<_>>();

    if !settings.ignore_curated_repository() {
        user_repo_ids.insert("com.vrchat.repos.curated".to_owned());
    }

    if !settings.ignore_official_repository() {
        user_repo_ids.insert("com.vrchat.repos.official".to_owned());
    }

    user_repo_ids
}

async fn download_one_repository(
    client: &impl HttpClient,
    repository_url: &Url,
    headers: &IndexMap<Box<str>, Box<str>>,
    user_repo_urls: &HashSet<String>,
    user_repo_ids: &HashSet<String>,
) -> Result<TauriDownloadRepository, RustError> {
    if user_repo_urls.contains(repository_url.as_str()) {
        return Ok(TauriDownloadRepository::Duplicated);
    }

    let repo = match RemoteRepository::download(client, repository_url, headers).await {
        Ok((repo, _)) => repo,
        Err(e) => {
            return Ok(TauriDownloadRepository::DownloadError {
                message: e.to_string(),
            });
        }
    };

    let url = repo.url().unwrap_or(repository_url).as_str();
    let id = repo.id().unwrap_or(url);

    if user_repo_ids.contains(id) {
        return Ok(TauriDownloadRepository::Duplicated);
    }

    Ok(TauriDownloadRepository::Success {
        value: TauriRemoteRepositoryInfo {
            id: id.to_string(),
            url: url.to_string(),
            display_name: repo.name().unwrap_or(id).to_string(),
            packages: repo
                .get_packages()
                .filter_map(|x| x.get_latest(VersionSelector::latest_for(None, true)))
                .filter(|x| !x.is_yanked())
                .map(TauriBasePackageInfo::new)
                .collect(),
        },
    })
}

#[derive(Serialize, specta::Type)]
pub enum TauriAddRepositoryResult {
    BadUrl,
    Success,
}

#[tauri::command]
#[specta::specta]
pub async fn environment_add_repository(
    settings: State<'_, SettingsState>,
    packages: State<'_, PackagesState>,
    io: State<'_, DefaultEnvironmentIo>,
    http: State<'_, reqwest::Client>,
    url: String,
    headers: IndexMapV2<Box<str>, Box<str>>,
) -> Result<TauriAddRepositoryResult, RustError> {
    let url: Url = match url.parse() {
        Err(_) => {
            return Ok(TauriAddRepositoryResult::BadUrl);
        }
        Ok(url) => url,
    };

    let mut settings = settings.load_mut(io.inner()).await?;
    add_remote_repo(
        &mut settings,
        url,
        None,
        headers.0,
        io.inner(),
        http.inner(),
    )
    .await?;
    settings.save().await?;

    // force update repository
    packages.clear_cache();

    Ok(TauriAddRepositoryResult::Success)
}

#[tauri::command]
#[specta::specta]
pub async fn environment_remove_repository(
    settings: State<'_, SettingsState>,
    packages: State<'_, PackagesState>,
    io: State<'_, DefaultEnvironmentIo>,
    id: String,
) -> Result<(), RustError> {
    let mut settings = settings.load_mut(io.inner()).await?;

    let removed = settings.remove_repo(|r| r.id() == Some(id.as_str()));

    join_all(
        removed
            .iter()
            .map(|x| async { io.remove_file(x.local_path()).await.ok() }),
    )
    .await;

    settings.save().await?;

    packages.clear_cache();

    Ok(())
}

#[derive(Serialize, specta::Type)]
#[serde(tag = "type")]
pub enum TauriImportRepositoryPickResult {
    NoFilePicked,
    ParsedRepositories {
        repositories: Vec<TauriRepositoryDescriptor>,
        unparsable_lines: Vec<String>,
    },
}

// workaround bug in specta::Type derive macro
type Headers = IndexMapV2<Box<str>, Box<str>>;

#[derive(Serialize, Deserialize, specta::Type, Clone)]
pub struct TauriRepositoryDescriptor {
    pub url: Url,
    pub headers: Headers,
}

#[tauri::command]
#[specta::specta]
pub async fn environment_import_repository_pick(
) -> Result<TauriImportRepositoryPickResult, RustError> {
    let builder = FileDialogBuilder::new();

    let Some(repositories_path) = builder.pick_file() else {
        return Ok(TauriImportRepositoryPickResult::NoFilePicked);
    };

    let repositories_file = tokio::fs::read_to_string(repositories_path).await?;

    let result = RepositoriesFile::parse(&repositories_file);

    Ok(TauriImportRepositoryPickResult::ParsedRepositories {
        repositories: result
            .parsed()
            .repositories()
            .iter()
            .map(|x| TauriRepositoryDescriptor {
                url: x.url().clone(),
                headers: IndexMapV2(x.headers().clone()),
            })
            .collect(),
        unparsable_lines: result.unparseable_lines().to_vec(),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn environment_import_download_repositories(
    window: Window,
    channel: String,
    repositories: Vec<TauriRepositoryDescriptor>,
) -> Result<
    AsyncCallResult<usize, Vec<(TauriRepositoryDescriptor, TauriDownloadRepository)>>,
    RustError,
> {
    async_command(channel, window.clone(), async move {
        With::<usize>::continue_async(|ctx| async move {
            let settings = window.state::<SettingsState>();
            let io = window.state::<DefaultEnvironmentIo>();
            let settings = settings.load(io.inner()).await?;
            {
                let user_repo_urls = user_repo_urls(&settings);
                let mut user_repo_ids = user_repo_ids(&settings);
                drop(settings);

                info!("downloading {} repositories", repositories.len());

                let counter = AtomicUsize::new(0);

                let counter_ref = &counter;
                let user_repo_urls_ref = &user_repo_urls;
                let user_repo_ids_ref = &user_repo_ids;

                let http = window.state::<reqwest::Client>();
                let mut results = try_join_all(repositories.into_iter().map(|adding_repo| {
                    let ctx = ctx.clone();
                    let http = http.clone();
                    async move {
                        let downloaded = download_one_repository(
                            http.inner(),
                            &adding_repo.url,
                            &adding_repo.headers.0,
                            user_repo_urls_ref,
                            user_repo_ids_ref,
                        )
                        .await?;

                        info!("downloaded repository: {:?}", adding_repo.url);

                        let count = counter_ref.fetch_add(1, Ordering::Relaxed);
                        ctx.emit(count).unwrap();

                        Ok::<_, RustError>((adding_repo, downloaded))
                    }
                }))
                .await?;

                for (_, downloaded) in results.as_mut_slice() {
                    if let TauriDownloadRepository::Success { value } = &downloaded {
                        if user_repo_ids.contains(&value.id) {
                            info!("duplicated repository in list: {}", value.url);
                            *downloaded = TauriDownloadRepository::Duplicated;
                        } else {
                            user_repo_ids.insert(value.id.to_string());
                        }
                    }
                }

                Ok(results)
            }
        })
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn environment_import_add_repositories(
    settings: State<'_, SettingsState>,
    packages: State<'_, PackagesState>,
    http: State<'_, reqwest::Client>,
    io: State<'_, DefaultEnvironmentIo>,
    repositories: Vec<TauriRepositoryDescriptor>,
) -> Result<(), RustError> {
    let mut settings = settings.load_mut(io.inner()).await?;
    for adding_repo in repositories {
        add_remote_repo(
            &mut settings,
            adding_repo.url,
            None,
            adding_repo.headers.0,
            io.inner(),
            http.inner(),
        )
        .await?;
    }
    settings.save().await?;

    // force update repository
    packages.clear_cache();

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn environment_export_repositories(
    settings: State<'_, SettingsState>,
    io: State<'_, DefaultEnvironmentIo>,
    window: Window,
) -> Result<(), RustError> {
    let Some(path) = window
        .dialog()
        .file()
        .set_parent(&window)
        .add_filter("Text", &["txt"])
        .set_file_name("repositories")
        .blocking_save_file()
    else {
        return Ok(());
    };

    let repositories = settings.load(io.inner()).await?.export_repositories();

    write(path, repositories).await?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn environment_clear_package_cache(
    packages: State<'_, PackagesState>,
    io: State<'_, DefaultEnvironmentIo>,
) -> Result<(), RustError> {
    clear_package_cache(io.inner()).await?;
    packages.clear_cache();

    Ok(())
}

#[derive(Serialize, specta::Type)]
pub struct TauriUserPackage {
    path: String,
    package: TauriBasePackageInfo,
}

#[tauri::command]
#[specta::specta]
pub async fn environment_get_user_packages(
    settings: State<'_, SettingsState>,
    io: State<'_, DefaultEnvironmentIo>,
) -> Result<Vec<TauriUserPackage>, RustError> {
    let settings = settings.load(io.inner()).await?;
    let packages = UserPackageCollection::load(&settings, io.inner()).await;

    Ok(packages
        .packages()
        .filter_map(|(path, json)| {
            let path = path.as_os_str().to_str()?;
            Some(TauriUserPackage {
                path: path.into(),
                package: TauriBasePackageInfo::new(json),
            })
        })
        .collect())
}

#[derive(Serialize, specta::Type)]
pub enum TauriAddUserPackageWithPickerResult {
    NoFolderSelected,
    InvalidSelection,
    AlreadyAdded,
    Successful,
}

#[tauri::command]
#[specta::specta]
pub async fn environment_add_user_package_with_picker(
    settings: State<'_, SettingsState>,
    packages: State<'_, PackagesState>,
    io: State<'_, DefaultEnvironmentIo>,
    window: Window,
) -> Result<TauriAddUserPackageWithPickerResult, RustError> {
    let Some(project_path) = window
        .dialog()
        .file()
        .set_parent(&window)
        .blocking_pick_folder()
    else {
        return Ok(TauriAddUserPackageWithPickerResult::NoFolderSelected);
    };

    let Ok(project_path) = project_path.into_os_string().into_string() else {
        return Ok(TauriAddUserPackageWithPickerResult::InvalidSelection);
    };

    {
        let mut settings = settings.load_mut(io.inner()).await?;
        match settings
            .add_user_package(project_path.as_ref(), io.inner())
            .await
        {
            AddUserPackageResult::Success => {}
            AddUserPackageResult::NonAbsolute => unreachable!("absolute path"),
            AddUserPackageResult::BadPackage => {
                return Ok(TauriAddUserPackageWithPickerResult::InvalidSelection);
            }
            AddUserPackageResult::AlreadyAdded => {
                return Ok(TauriAddUserPackageWithPickerResult::AlreadyAdded);
            }
        }

        settings.save().await?;
    }

    packages.clear_cache();

    Ok(TauriAddUserPackageWithPickerResult::Successful)
}

#[tauri::command]
#[specta::specta]
pub async fn environment_remove_user_packages(
    settings: State<'_, SettingsState>,
    packages: State<'_, PackagesState>,
    io: State<'_, DefaultEnvironmentIo>,
    path: String,
) -> Result<(), RustError> {
    {
        let mut settings = settings.load_mut(io.inner()).await?;
        settings.remove_user_package(Path::new(&path));
        settings.save().await?;
    }

    packages.clear_cache();

    Ok(())
}
