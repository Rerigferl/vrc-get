//! This module contains vpm core implementation
//!
//! This module might be a separated crate.

use crate::version::{Version, VersionRange};
use crate::vpm::structs::manifest::{VpmDependency, VpmLockedDependency};
use crate::vpm::structs::package::PackageJson;
use crate::vpm::structs::remote_repo::PackageVersions;
use crate::vpm::structs::repository::LocalCachedRepository;
use crate::vpm::structs::setting::UserRepoSetting;
use futures::future::{join_all, try_join_all};
use futures::prelude::*;
use indexmap::IndexMap;
use itertools::Itertools as _;
use reqwest::{Client, IntoUrl, Url};
use serde_json::{from_value, to_value, Map, Value};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::future::ready;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::{env, fmt, io};
use tokio::fs::{create_dir_all, read_dir, remove_dir_all, remove_file, File, OpenOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use utils::*;

mod repo_holder;
pub mod structs;
mod utils;

use repo_holder::RepoHolder;

type JsonMap = Map<String, Value>;

/// This struct holds global state (will be saved on %LOCALAPPDATA% of VPM.
#[derive(Debug)]
pub struct Environment {
    http: Client,
    /// config folder.
    /// On windows, `%APPDATA%\\VRChatCreatorCompanion`.
    /// On posix, `${XDG_DATA_HOME}/VRChatCreatorCompanion`.
    global_dir: PathBuf,
    /// parsed settings
    settings: Map<String, Value>,
    /// Cache
    repo_cache: RepoHolder,
    settings_changed: bool,
}

impl Environment {
    pub async fn load_default(http: Client) -> io::Result<Environment> {
        let mut folder = Environment::get_local_config_folder();
        folder.push("VRChatCreatorCompanion");
        let folder = folder;

        Ok(Environment {
            http: http.clone(),
            settings: load_json_or_default(&folder.join("settings.json")).await?,
            global_dir: folder,
            repo_cache: RepoHolder::new(http),
            settings_changed: false,
        })
    }

    #[cfg(windows)]
    fn get_local_config_folder() -> PathBuf {
        // use CLSID?
        if let Some(local_appdata) = env::var_os("CSIDL_LOCAL_APPDATA") {
            return local_appdata.into();
        }
        // fallback: use HOME
        if let Some(home_folder) = env::var_os("HOMEPATH") {
            let mut path = PathBuf::from(home_folder);
            path.push("AppData\\Local");
            return path;
        }

        panic!("no CSIDL_LOCAL_APPDATA nor HOMEPATH are set!")
    }

    #[cfg(not(windows))]
    fn get_local_config_folder() -> PathBuf {
        if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
            return data_home.into();
        }

        // fallback: use HOME
        if let Some(home_folder) = env::var_os("HOME") {
            let mut path = PathBuf::from(home_folder);
            path.push(".local/share");
            return path;
        }

        panic!("no XDG_DATA_HOME nor HOME are set!")
    }

    pub(crate) fn get_repos_dir(&self) -> PathBuf {
        self.global_dir.join("Repos")
    }

    pub async fn find_package_by_name<'a>(
        &self,
        package: &str,
        version: VersionSelector<'a>,
    ) -> io::Result<Option<PackageJson>> {
        let mut versions = self.find_packages(package).await?;

        versions.sort_by(|a, b| a.version.cmp(&b.version));

        Ok(versions
            .into_iter()
            .filter(|x| version.satisfies(&x.version))
            .next())
    }

    pub async fn get_repo_sources(&self) -> io::Result<Vec<RepoSource>> {
        // collect user repositories for get_repos_dir
        let repos_base = self.get_repos_dir();
        let user_repos = self.get_user_repos()?;

        let mut user_repo_file_names = HashSet::new();
        user_repo_file_names.insert(OsStr::new("vrc-curated.json"));
        user_repo_file_names.insert(OsStr::new("vrc-official.json"));

        fn relative_file_name<'a>(path: &'a Path, base: &Path) -> Option<&'a OsStr> {
            path.strip_prefix(&base)
                .ok()
                .filter(|x| x.parent().map(|x| x.as_os_str().is_empty()).unwrap_or(true))
                .and_then(|x| x.file_name())
        }

        user_repo_file_names.extend(
            user_repos
                .iter()
                .filter_map(|x| relative_file_name(&x.local_path, &repos_base)),
        );

        let mut entry = read_dir(self.get_repos_dir()).await?;
        let streams = stream::poll_fn(|cx| {
            entry.poll_next_entry(cx).map(|x| match x {
                Ok(Some(v)) => Some(Ok(v)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            })
        });

        let undefined_repos = streams
            .map_ok(|x| x.path())
            .try_filter(|x| ready(!user_repo_file_names.contains(x.file_name().unwrap())))
            .try_filter(|x| ready(x.extension() == Some(OsStr::new("json"))))
            .try_filter(|x| {
                tokio::fs::metadata(x.clone()).map(|x| x.map(|x| x.is_file()).unwrap_or(false))
            })
            .map_ok(RepoSource::Undefined);

        let defined_sources = DEFINED_REPO_SOURCES
            .into_iter()
            .copied()
            .map(RepoSource::PreDefined);
        let user_repo_sources = self.get_user_repos()?.into_iter().map(RepoSource::UserRepo);

        stream::iter(defined_sources.chain(user_repo_sources).map(Ok))
            .chain(undefined_repos)
            .try_collect::<Vec<_>>()
            .await
    }

    pub async fn get_repos(&self) -> io::Result<Vec<&LocalCachedRepository>> {
        try_join_all(
            error_flatten(self.get_repo_sources().await)
                .map_ok(|x| self.get_repo(x))
                .map(|x| async move {
                    match x {
                        Ok(f) => f.await,
                        Err(e) => Err(e),
                    }
                }),
        )
        .await
    }

    async fn get_repo(&self, source: RepoSource) -> io::Result<&LocalCachedRepository> {
        match source {
            RepoSource::PreDefined(source) => {
                self.repo_cache
                    .get_or_create_repo(
                        &self.get_repos_dir().joined(source.file_name),
                        source.url,
                        Some(source.name),
                    )
                    .await
            }
            RepoSource::UserRepo(user_repo) => self.repo_cache.get_user_repo(&user_repo).await,
            RepoSource::Undefined(repo_json) => {
                self.repo_cache
                    .get_repo(&repo_json, || async { unreachable!() })
                    .await
            }
        }
    }

    pub(crate) async fn find_packages(&self, package: &str) -> io::Result<Vec<PackageJson>> {
        let mut list = Vec::new();

        fn packages<'a>(
            package: &str,
            repo: &'a LocalCachedRepository,
        ) -> impl Iterator<Item = serde_json::Result<PackageJson>> + 'a {
            repo.cache
                .get(package)
                .into_iter()
                .map(|x| from_value::<PackageVersions>(x.clone()))
                .map_ok(|x| x.versions.into_values())
                .flatten_ok()
        }

        fn append<'a>(
            list: &mut Vec<PackageJson>,
            package: &str,
            repo: LocalCachedRepository,
        ) -> io::Result<()> {
            if let Some(version) = repo.cache.get(package) {
                list.extend(
                    from_value::<PackageVersions>(version.clone())?
                        .versions
                        .into_values(),
                );
            }
            Ok(())
        }

        self.get_repos()
            .await?
            .into_iter()
            .map(|repo| repo.cache.get(package).map(Clone::clone))
            .flatten()
            .map(|x| from_value::<PackageVersions>(x).map_err(io::Error::from))
            .map_ok(|x| x.versions.into_values())
            .flatten_ok()
            .fold_ok((), |_, pkg| list.push(pkg))?;

        // user package folders
        for x in self.get_user_package_folders()? {
            if let Some(package_json) =
                load_json_or_default::<Option<PackageJson>>(&x.joined("package.json")).await?
            {
                if package_json.name == package {
                    list.push(package_json);
                }
            }
        }

        Ok(list)
    }

    pub async fn add_package(
        &self,
        package: &PackageJson,
        target_packages_folder: &Path,
    ) -> io::Result<()> {
        let zip_path = {
            let mut building = self.global_dir.clone();
            building.push("Repos");
            building.push(&package.name);
            create_dir_all(&building).await?;
            building.push(&format!("{}-{}.zip", &package.name, &package.version));
            building
        };
        let dest_folder = target_packages_folder.join(&package.name);

        let zip_file = if let Some(cache_file) = try_open_file(&zip_path).await? {
            cache_file
        } else {
            // file not found: err
            let mut cache_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&zip_path)
                .await?;

            // TODO: streaming
            let got_data = self
                .http
                .get(&package.url)
                .send()
                .await
                .err_mapped()?
                .error_for_status()
                .err_mapped()?
                .bytes()
                .await
                .err_mapped()?;
            cache_file.write_all(&got_data).await?;
            cache_file.flush().await?;
            cache_file.seek(SeekFrom::Start(0)).await?;
            cache_file
        };

        // remove dest folder before extract if exists
        remove_dir_all(&dest_folder).await.ok();

        // extract zip file
        // TODO: sanitize to prevent directory traversal
        let mut zip_reader = async_zip::read::seek::ZipFileReader::new(zip_file)
            .await
            .err_mapped()?;
        for i in 0..zip_reader.file().entries().len() {
            let entry = zip_reader.file().entries()[i].entry();
            let path = dest_folder.join(entry.filename());
            if entry.dir() {
                // if it's directory, just create directory
                create_dir_all(path).await?;
            } else {
                let mut reader = zip_reader.entry(i).await.err_mapped()?;
                create_dir_all(path.parent().unwrap()).await?;
                let mut dest_file = File::create(path).await?;
                tokio::io::copy(&mut reader, &mut dest_file).await?;
                dest_file.flush().await?;
            }
        }

        Ok(())
    }

    pub(crate) fn get_user_repos(&self) -> serde_json::Result<Vec<UserRepoSetting>> {
        from_value::<Vec<UserRepoSetting>>(
            self.settings
                .get("userRepos")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
    }

    fn get_user_package_folders(&self) -> serde_json::Result<Vec<PathBuf>> {
        from_value(
            self.settings
                .get("userPackageFolders")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
    }

    fn add_user_repo(&mut self, repo: &UserRepoSetting) -> serde_json::Result<()> {
        self.settings
            .get_or_put_mut("user_repos", || Vec::<Value>::new())
            .as_array_mut()
            .expect("user_repos must be array")
            .push(to_value(repo)?);
        self.settings_changed = true;
        Ok(())
    }

    pub async fn add_remote_repo(
        &mut self,
        url: Url,
        name: Option<&str>,
    ) -> Result<(), AddRepositoryErr> {
        let user_repos = self.get_user_repos()?;
        if user_repos
            .iter()
            .any(|x| x.url.as_deref() == Some(url.as_ref()))
        {
            return Err(AddRepositoryErr::AlreadyAdded);
        }
        let remote_repo = download_remote_repository(&self.http, url.clone()).await?;
        let local_path = self
            .get_repos_dir()
            .joined(format!("{}.json", uuid::Uuid::new_v4()));

        let repo_name = name.map(str::to_owned).or_else(|| {
            remote_repo
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });

        let mut local_cache = LocalCachedRepository::new(
            local_path.clone(),
            repo_name.clone(),
            Some(url.to_string()),
        );
        local_cache.cache = remote_repo
            .get("packages")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or(JsonMap::new());
        local_cache.repo = Some(remote_repo);

        write_repo(&local_path, &local_cache).await?;

        self.add_user_repo(&UserRepoSetting::new(
            local_path.clone(),
            repo_name,
            Some(url.to_string()),
        ))?;
        Ok(())
    }

    pub async fn add_local_repo(
        &mut self,
        path: &Path,
        name: Option<&str>,
    ) -> Result<(), AddRepositoryErr> {
        let user_repos = self.get_user_repos()?;
        if user_repos.iter().any(|x| x.local_path.as_path() == path) {
            return Err(AddRepositoryErr::AlreadyAdded);
        }

        self.add_user_repo(&UserRepoSetting::new(
            path.to_owned(),
            name.map(str::to_owned),
            None,
        ))?;
        Ok(())
    }

    pub async fn remove_repo(
        &mut self,
        condition: impl Fn(&UserRepoSetting) -> bool,
    ) -> io::Result<bool> {
        let user_repos = self.get_user_repos()?;
        let mut indices = user_repos
            .iter()
            .enumerate()
            .filter(|(_, x)| condition(x))
            .collect::<Vec<_>>();
        indices.reverse();
        if indices.len() == 0 {
            return Ok(false);
        }

        let repos_json = self
            .settings
            .get_mut("userRepos")
            .and_then(Value::as_array_mut)
            .expect("userRepos");

        for (i, _) in &indices {
            repos_json.remove(*i);
        }

        join_all(indices.iter().map(|(_, x)| remove_file(&x.local_path))).await;
        self.settings_changed = true;
        Ok(true)
    }

    pub async fn save(&mut self) -> io::Result<()> {
        if !self.settings_changed {
            return Ok(());
        }

        let mut file = File::create(self.global_dir.join("settings.json")).await?;
        file.write_all(&to_json_vec(&self.settings)?).await?;
        file.flush().await?;
        self.settings_changed = false;
        Ok(())
    }
}

#[derive(Copy, Clone)]
pub struct PreDefinedRepoSource {
    file_name: &'static str,
    url: &'static str,
    name: &'static str,
}

#[derive(Clone)]
#[non_exhaustive]
pub enum RepoSource {
    PreDefined(PreDefinedRepoSource),
    UserRepo(UserRepoSetting),
    Undefined(PathBuf),
}

static OFFICIAL_REPO_SOURCE: PreDefinedRepoSource = PreDefinedRepoSource {
    file_name: "vrc-official.json",
    url: "https://packages.vrchat.com/official?download",
    name: "Official",
};

static CURATED_REPO_SOURCE: PreDefinedRepoSource = PreDefinedRepoSource {
    file_name: "vrc-curated.json",
    url: "https://packages.vrchat.com/curated?download",
    name: "Curated",
};

static DEFINED_REPO_SOURCES: &[PreDefinedRepoSource] = &[OFFICIAL_REPO_SOURCE, CURATED_REPO_SOURCE];

#[derive(Debug)]
pub enum AddRepositoryErr {
    Io(io::Error),
    AlreadyAdded,
}

impl fmt::Display for AddRepositoryErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddRepositoryErr::Io(ioerr) => fmt::Display::fmt(ioerr, f),
            AddRepositoryErr::AlreadyAdded => f.write_str("already newer package installed"),
        }
    }
}

impl std::error::Error for AddRepositoryErr {}

impl From<io::Error> for AddRepositoryErr {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for AddRepositoryErr {
    fn from(value: serde_json::Error) -> Self {
        Self::Io(value.into())
    }
}

async fn update_from_remote(client: &Client, path: &Path, repo: &mut LocalCachedRepository) {
    let Some(remote_url) = repo.creation_info.as_ref().and_then(|x| x.url.as_ref()) else {
        return
    };

    match download_remote_repository(&client, remote_url).await {
        Ok(remote_repo) => {
            repo.cache = remote_repo
                .get("packages")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or(JsonMap::new());
            repo.repo = Some(remote_repo);
        }
        Err(e) => {
            log::error!("fetching remote repo '{}': {}", remote_url, e);
        }
    }

    match write_repo(path, repo).await {
        Ok(_) => {}
        Err(e) => {
            log::error!("writing local repo '{}': {}", path.display(), e);
        }
    }
}

async fn write_repo(path: &Path, repo: &LocalCachedRepository) -> io::Result<()> {
    let mut file = File::create(path).await?;
    file.write_all(&to_json_vec(repo)?).await?;
    file.flush().await?;
    Ok(())
}

pub(crate) async fn download_remote_repository(
    client: &Client,
    url: impl IntoUrl,
) -> io::Result<JsonMap> {
    fn map_err(err: reqwest::Error) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, err)
    }
    client
        .get(url)
        .send()
        .await
        .err_mapped()?
        .error_for_status()
        .err_mapped()?
        .json()
        .await
        .err_mapped()
}

use vpm_manifest::VpmManifest;
mod vpm_manifest {
    use super::*;
    use serde::Serialize;
    use serde_json::json;

    #[derive(Debug)]
    pub(super) struct VpmManifest {
        json: JsonMap,
        dependencies: IndexMap<String, VpmDependency>,
        locked: IndexMap<String, VpmLockedDependency>,
        changed: bool,
    }

    impl VpmManifest {
        pub(super) fn new(json: JsonMap) -> serde_json::Result<Self> {
            Ok(Self {
                dependencies: from_value(
                    json.get("dependencies")
                        .cloned()
                        .unwrap_or(Value::Object(JsonMap::new())),
                )?,
                locked: from_value(
                    json.get("locked")
                        .cloned()
                        .unwrap_or(Value::Object(JsonMap::new())),
                )?,
                json,
                changed: false,
            })
        }

        pub(super) fn dependencies(&self) -> &IndexMap<String, VpmDependency> {
            &self.dependencies
        }

        pub(super) fn locked(&self) -> &IndexMap<String, VpmLockedDependency> {
            &self.locked
        }

        pub(super) fn add_dependency(&mut self, name: &str, dependency: VpmDependency) {
            // update both parsed and non-parsed
            self.add_value("dependencies", name, &dependency);
            self.dependencies.insert(name.to_string(), dependency);
        }

        pub(super) fn add_locked(&mut self, name: &str, dependency: VpmLockedDependency) {
            // update both parsed and non-parsed
            self.add_value("locked", name, &dependency);
            self.locked.insert(name.to_string(), dependency);
        }

        fn add_value(&mut self, key0: &str, key1: &str, value: &impl Serialize) {
            let serialized = to_value(value).expect("serialize err");
            match self.json.get_mut(key0) {
                Some(Value::Object(obj)) => {
                    obj.insert(key1.to_string(), serialized);
                }
                _ => {
                    self.json.insert(key0.into(), json!({ key1: serialized }));
                }
            }
            self.changed = true;
        }

        pub(super) async fn save_to(&self, file: &Path) -> io::Result<()> {
            if !self.changed {
                return Ok(());
            }
            let mut file = File::create(file).await?;
            file.write_all(&to_json_vec(&self.json)?).await?;
            file.flush().await?;
            Ok(())
        }
    }
}

#[derive(Debug)]
pub struct UnityProject {
    /// path to `Packages` folder.
    packages_dir: PathBuf,
    /// manifest.json
    manifest: VpmManifest,
}

impl UnityProject {
    pub async fn find_unity_project(unity_project: Option<PathBuf>) -> io::Result<UnityProject> {
        let mut unity_found = unity_project
            .ok_or(())
            .or_else(|_| UnityProject::find_unity_project_path())?;
        unity_found.push("Packages");

        let manifest = unity_found.join("vpm-manifest.json");

        Ok(UnityProject {
            packages_dir: unity_found,
            manifest: VpmManifest::new(load_json_or_default(&manifest).await?)?,
        })
    }

    fn find_unity_project_path() -> io::Result<PathBuf> {
        let mut candidate = env::current_dir()?;

        loop {
            candidate.push("Packages");
            candidate.push("vpm-manifest.json");

            if candidate.exists() {
                // if there's vpm-manifest.json, it's project path
                candidate.pop();
                candidate.pop();
                return Ok(candidate);
            }

            // replace vpm-manifest.json -> manifest.json
            candidate.pop();
            candidate.push("manifest.json");

            if candidate.exists() {
                // if there's manifest.json (which is manifest.json), it's project path
                candidate.pop();
                candidate.pop();
                return Ok(candidate);
            }

            // remove Packages/manifest.json
            candidate.pop();
            candidate.pop();

            // go to parent dir
            if !candidate.pop() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Unity project Not Found",
                ));
            }
        }
    }

    /// Add specified package to self project.
    ///
    /// If the package or newer one is already installed in dependencies, this does nothing
    /// and returns AlreadyNewerPackageInstalled err.
    ///
    /// If the package or newer one is already installed in locked list,
    /// this adds specified (not locked) version to dependencies
    pub async fn add_package(
        &mut self,
        env: &Environment,
        request: &PackageJson,
    ) -> Result<(), AddPackageErr> {
        use crate::vpm::AddPackageErr::*;
        // if same or newer requested package is in dependencies, do nothing
        if let Some(dep) = self.manifest.dependencies().get(&request.name) {
            if dep.version >= request.version {
                return Err(AlreadyNewerPackageInstalled);
            }
        }

        // if same or newer requested package is in locked dependencies,
        // just add requested version into dependencies
        if let Some(locked) = self.manifest.locked().get(&request.name) {
            if locked.version >= request.version {
                self.manifest
                    .add_dependency(&request.name, VpmDependency::new(request.version.clone()));
                return Ok(());
            }
        }

        // check for version conflict
        self.check_conflict(&request.name, &request.version)?;

        let adding_deps = self.collect_adding_packages(env, request).await?;

        // check for version conflict for all deps
        for x in &adding_deps {
            self.check_conflict(&x.name, &x.version)?;
        }

        let mut packages = adding_deps.iter().collect::<Vec<_>>();
        packages.push(request);
        let packages = packages;

        // there's no errors to add package. adding to dependencies

        // first, add to dependencies
        self.manifest
            .add_dependency(&request.name, VpmDependency::new(request.version.clone()));

        // then, lock all dependencies
        for pkg in packages.iter() {
            self.manifest.add_locked(
                &request.name,
                VpmLockedDependency::new(
                    pkg.version.clone(),
                    pkg.vpm_dependencies.clone().unwrap_or_else(IndexMap::new),
                ),
            );
        }

        // resolve all packages
        let futures = packages
            .iter()
            .map(|x| env.add_package(x, &self.packages_dir))
            .collect::<Vec<_>>();
        for x in join_all(futures).await {
            x?;
        }

        Ok(())
    }

    async fn collect_adding_packages(
        &mut self,
        env: &Environment,
        pkg: &PackageJson,
    ) -> Result<Vec<PackageJson>, AddPackageErr> {
        let mut all_deps = Vec::new();
        let mut adding_deps = Vec::new();
        self.collect_adding_packages_internal(&mut all_deps, env, pkg)
            .await?;
        let mut i = 0;
        while i < all_deps.len() {
            self.collect_adding_packages_internal(&mut adding_deps, env, &all_deps[i])
                .await?;
            all_deps.append(&mut adding_deps);
            i += 1;
        }
        Ok(all_deps)
    }

    async fn collect_adding_packages_internal(
        &mut self,
        adding_deps: &mut Vec<PackageJson>,
        env: &Environment,
        pkg: &PackageJson,
    ) -> Result<(), AddPackageErr> {
        if let Some(dependencies) = &pkg.vpm_dependencies {
            for (dep, range) in dependencies {
                if self
                    .manifest
                    .locked()
                    .get(dep)
                    .map(|x| range.matches(&x.version))
                    .unwrap_or(false)
                {
                    let found = env
                        .find_package_by_name(dep, VersionSelector::Range(range))
                        .await?;
                    adding_deps.push(found.ok_or_else(|| AddPackageErr::DependencyNotFound {
                        dependency_name: dep.clone(),
                    })?);
                }
            }
        }
        Ok(())
    }

    fn check_conflict(&self, name: &str, version: &Version) -> Result<(), AddPackageErr> {
        for (pkg_name, locked) in self.manifest.locked() {
            if let Some(dep) = locked.dependencies.get(name) {
                if !dep.matches(&version) {
                    return Err(AddPackageErr::ConflictWithDependencies {
                        conflict: name.to_owned(),
                        dependency_name: pkg_name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub async fn save(&mut self) -> io::Result<()> {
        self.manifest
            .save_to(&self.packages_dir.join("vpm-manifest.json"))
            .await
    }

    pub async fn resolve(&self, env: &Environment) -> io::Result<()> {
        for (pkg, dep) in self.manifest.locked() {
            let pkg = env
                .find_package_by_name(&pkg, VersionSelector::Specific(&dep.version))
                .await?
                .expect("some package in manifest.json not found");
            env.add_package(&pkg, &self.packages_dir).await?;
        }
        Ok(())
    }
}

pub enum VersionSelector<'a> {
    Latest,
    LatestIncluidingPrerelease,
    Specific(&'a Version),
    Range(&'a VersionRange),
}

impl<'a> VersionSelector<'a> {
    pub fn satisfies(&self, version: &Version) -> bool {
        match self {
            VersionSelector::Latest => version.pre.is_empty(),
            VersionSelector::LatestIncluidingPrerelease => true,
            VersionSelector::Specific(finding) => &version == finding,
            VersionSelector::Range(range) => range.matches(version),
        }
    }
}

#[derive(Debug)]
pub enum AddPackageErr {
    Io(io::Error),
    AlreadyNewerPackageInstalled,
    ConflictWithDependencies {
        /// conflicting package name
        conflict: String,
        /// the name of locked package
        dependency_name: String,
    },
    DependencyNotFound {
        dependency_name: String,
    },
}

impl fmt::Display for AddPackageErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddPackageErr::Io(ioerr) => fmt::Display::fmt(ioerr, f),
            AddPackageErr::AlreadyNewerPackageInstalled => {
                f.write_str("already newer package installed")
            }
            AddPackageErr::ConflictWithDependencies {
                conflict,
                dependency_name,
            } => write!(f, "{conflict} conflicts with {dependency_name}"),
            AddPackageErr::DependencyNotFound { dependency_name } => write!(
                f,
                "Package {dependency_name} (maybe dependencies of the package) not found"
            ),
        }
    }
}

impl std::error::Error for AddPackageErr {}

impl From<io::Error> for AddPackageErr {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// open file or returns none if not exists
async fn try_open_file(path: &Path) -> io::Result<Option<File>> {
    match File::open(path).await {
        Ok(file) => Ok(Some(file)),
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

async fn load_json_or_else<T>(
    manifest_path: &Path,
    default: impl FnOnce() -> io::Result<T>,
) -> io::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    match try_open_file(manifest_path).await? {
        Some(file) => Ok(serde_json::from_slice(&read_to_vec(file).await?)?),
        None => default(),
    }
}

async fn load_json_or_default<T>(manifest_path: &Path) -> io::Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    load_json_or_else(manifest_path, || Ok(Default::default())).await
}

async fn read_to_vec(mut read: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut vec = Vec::new();
    read.read_to_end(&mut vec).await?;
    Ok(vec)
}

fn to_json_vec<T>(value: &T) -> serde_json::Result<Vec<u8>>
where
    T: ?Sized + serde::Serialize,
{
    serde_json::to_vec_pretty(value)
}