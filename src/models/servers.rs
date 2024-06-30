use log::{debug, error, trace};
use serde::{Deserialize, Serialize};

use crate::errors::{CVMFSScraperError, GenericError, ManifestError, ScrapeError};
use crate::models::cvmfs_published::Manifest;
use crate::models::cvmfs_status_json::StatusJSON;
use crate::models::generic::{Hostname, MaybeRfc2822DateTime};
use crate::models::meta_json::MetaJSON;
use crate::models::repositories_json::RepositoriesJSON;
use crate::utilities::fetch_json;

/// The type of server we're dealing with.
///
/// Stratum0: The main server that holds the master copy of the data.
/// Stratum1: A server that holds a copy of the data from the Stratum0 server.
/// SyncServer: A server that holds a copy of the data from the Stratum0 server, but is not a Stratum1 server.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Copy)]
pub enum ServerType {
    Stratum0,
    Stratum1,
    SyncServer,
}

/// The type of backend the server is using.
///
/// S3: The server is using S3 as the backend.
/// CVMFS: The server is using a standard CVMFS web server as the backend.
/// AutoDetect: The server will try to detect the backend type.
///
/// The AutoDetect backend type will try to fetch the repositories.json file from the server. If it
/// fails, it will assume the server is using S3 as the backend. If it succeeds, it will assume the
/// server is using CVMFS as the backend.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Copy)]
pub enum ServerBackendType {
    S3,
    CVMFS,
    AutoDetect,
}

/// A server object.
///
/// This object represents a CVMFS server. It contains the server type, the backend type, and the
/// hostname of the server.
///
/// The server object can be used to scrape the server for information about the repositories it
/// hosts. The scrape method will return a populated server object that contains information about
/// the server and the repositories it hosts.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Server {
    pub server_type: ServerType,
    #[serde(default = "default_backend_type")]
    pub backend_type: ServerBackendType,
    pub hostname: Hostname,
}

fn default_backend_type() -> ServerBackendType {
    ServerBackendType::AutoDetect
}

/// A populated server object.
///
/// This type is not to be manually created, but is the result of scraping a server object.
///
/// This object represents a CVMFS server that has been scraped for information about the repositories
/// it hosts. Note that replicas and repositories are consolidated into the attribute "repositories" as
/// they are functionally the same and no server will have both.
///
/// Fields:
///
/// - server_type: The server type (Stratum0, Stratum1, or SyncServer)
/// - backend_type: The backend type (S3, CVMFS, or AutoDetect)
/// - backend_detected: The detected backend type (S3 or CVMFS), will never be AutoDetect.
/// - hostname: The hostname of the server
/// - repositories: A list of populated repositories (or replicas)
/// - metadata: Metadata about the server (merged from repositories.json and meta.json, if found).
///
/// Metadata is not available servers using S3 as the backend as they do not provide repositories.json
#[derive(Debug, Clone, PartialEq)]
pub struct PopulatedServer {
    pub server_type: ServerType,
    pub backend_type: ServerBackendType,
    pub backend_detected: ServerBackendType,
    pub hostname: Hostname,
    pub repositories: Vec<PopulatedRepositoryOrReplica>,
    pub metadata: ServerMetadata,
}

/// A server that failed to scrape.
///
/// This struct is used to store information about a server that failed to scrape. It contains the
/// hostname of the server and the error that occurred.
#[derive(Debug, Clone)]
pub struct FailedServer {
    pub hostname: Hostname,
    pub server_type: ServerType,
    pub backend_type: ServerBackendType,
    pub error: CVMFSScraperError,
}

#[derive(Debug, Clone)]
pub enum ScrapedServer {
    Populated(PopulatedServer),
    Failed(FailedServer),
}

impl ScrapedServer {
    pub fn is_failed(&self) -> bool {
        matches!(self, ScrapedServer::Failed(_))
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, ScrapedServer::Populated(_))
    }
    pub fn get_populated_server(self) -> Result<PopulatedServer, GenericError> {
        match self {
            ScrapedServer::Populated(server) => Ok(server),
            ScrapedServer::Failed(failed) => Err(GenericError::TypeError(format!(
                "{} is a failed server",
                failed.hostname
            ))),
        }
    }
    pub fn get_failed_server(self) -> Result<FailedServer, GenericError> {
        match self {
            ScrapedServer::Failed(failed) => Ok(failed),
            ScrapedServer::Populated(server) => Err(GenericError::TypeError(format!(
                "{} is a populated server",
                server.hostname
            ))),
        }
    }
}

impl Server {
    pub fn new(
        server_type: ServerType,
        backend_type: ServerBackendType,
        hostname: Hostname,
    ) -> Self {
        trace!("Creating server object for {}", hostname.0);
        Server {
            server_type,
            backend_type,
            hostname,
        }
    }

    pub fn as_failed_server(&self, error: CVMFSScraperError) -> FailedServer {
        FailedServer {
            hostname: self.hostname.clone(),
            server_type: self.server_type,
            backend_type: self.backend_type,
            error,
        }
    }

    pub async fn scrape<R>(&self, repositories: Vec<R>) -> ScrapedServer
    where
        R: AsRef<str> + std::fmt::Display + Clone,
    {
        debug!("Scraping server {}", self.hostname.0);
        let client = reqwest::Client::new();
        let mut all_repos = repositories
            .iter()
            .map(|repo| repo.to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let mut populated_repos = vec![];
        let mut backend_detected = self.backend_type;

        let mut metadata = MetadataFromRepoJSON {
            schema_version: None,
            cvmfs_version: None,
            last_geodb_update: MaybeRfc2822DateTime(None),
            os_version_id: None,
            os_pretty_name: None,
            os_id: None,
        };

        // Backend type behavior when dealing with repos from http://servername/info/v1/repositories.json
        // AutoDetect: Try to fetch the repositories.json, if it fails, assume we're on S3 and
        //             scrape the repositories provided. Accept fetch failures, and accept an empty list.
        // S3: Scrape the repositories provided. Raise an error if the list is empty.
        // CMVFS: Fetch the repositories.json and merge it with the repositories provided. Raise an error
        //        if the fetch fails.

        match self.backend_type {
            ServerBackendType::AutoDetect => match self.fetch_repos_json(&client).await {
                Ok(repo_json) => {
                    debug!("Detected CVMFS backend for {}", self.hostname.0);
                    match self.validate_repo_json_and_server_type(&repo_json) {
                        Ok(_) => {}
                        Err(error) => return ScrapedServer::Failed(self.as_failed_server(error)),
                    }
                    metadata = match MetadataFromRepoJSON::try_from(repo_json.clone()) {
                        Ok(meta) => meta,
                        Err(error) => {
                            return ScrapedServer::Failed(self.as_failed_server(error.into()))
                        }
                    };
                    backend_detected = ServerBackendType::CVMFS;
                    all_repos.extend(
                        repo_json
                            .repositories_and_replicas()
                            .into_iter()
                            .map(|r| r.name),
                    );
                }
                Err(error) => match error {
                    ScrapeError::FetchError(_) => {
                        debug!("Detected S3 backend for {}", self.hostname.0);
                        backend_detected = ServerBackendType::S3;
                    }
                    _ => return ScrapedServer::Failed(self.as_failed_server(error.into())),
                },
            },
            ServerBackendType::S3 => {
                if all_repos.is_empty() {
                    error!(
                        "Empty repository list with explicit S3 backend: {}",
                        self.hostname.0
                    );
                    return ScrapedServer::Failed(self.as_failed_server(
                        ScrapeError::EmptyRepositoryList(self.hostname.0.clone()).into(),
                    ));
                }
            }
            ServerBackendType::CVMFS => {
                let repo_json = match self.fetch_repos_json(&client).await {
                    Ok(repo_json) => repo_json,
                    Err(error) => {
                        return ScrapedServer::Failed(self.as_failed_server(error.into()))
                    }
                };
                metadata = match MetadataFromRepoJSON::try_from(repo_json.clone()) {
                    Ok(meta) => meta,
                    Err(error) => {
                        return ScrapedServer::Failed(self.as_failed_server(error.into()))
                    }
                };
                match self.validate_repo_json_and_server_type(&repo_json) {
                    Ok(_) => {}
                    Err(error) => {
                        return ScrapedServer::Failed(self.as_failed_server(error));
                    }
                }
                all_repos.extend(
                    repo_json
                        .repositories_and_replicas()
                        .into_iter()
                        .map(|r| r.name),
                );
            }
        }

        for repo in all_repos {
            let repo = RepositoryOrReplica::new(&repo, self);
            let populated_repo = match repo.scrape(&client).await {
                Ok(repo) => repo,
                Err(error) => {
                    return ScrapedServer::Failed(self.as_failed_server(error));
                }
            };
            populated_repos.push(populated_repo);
        }

        let meta_json: Option<MetaJSON> = match self.fetch_meta_json(&client).await {
            Ok(meta) => Some(meta),
            Err(_) => None,
        };

        let metadata = self.merge_metadata(metadata, meta_json);

        ScrapedServer::Populated(PopulatedServer {
            server_type: self.server_type,
            backend_type: self.backend_type,
            backend_detected,
            hostname: self.hostname.clone(),
            repositories: populated_repos,
            metadata,
        })
    }

    async fn fetch_repos_json(
        &self,
        client: &reqwest::Client,
    ) -> Result<RepositoriesJSON, ScrapeError> {
        fetch_json(
            client,
            format!("http://{}/cvmfs/info/v1/repositories.json", self.hostname.0),
        )
        .await
    }

    async fn fetch_meta_json(&self, client: &reqwest::Client) -> Result<MetaJSON, ScrapeError> {
        fetch_json(
            client,
            format!("http://{}/cvmfs/info/v1/meta.json", self.hostname.0),
        )
        .await
    }

    fn validate_repo_json_and_server_type(
        &self,
        repo_json: &RepositoriesJSON,
    ) -> Result<(), CVMFSScraperError> {
        trace!("Validating {}", self.hostname.0);
        match (self.server_type, repo_json.replicas.is_empty()) {
            (ServerType::Stratum0, false) => Err(CVMFSScraperError::ScrapeError(
                ScrapeError::ServerTypeMismatch(format!(
                    "{} is a Stratum0 server, but replicas were found in the repositories.json",
                    self.hostname.0
                )),
            )),
            (ServerType::Stratum1, true) => Err(CVMFSScraperError::ScrapeError(
                ScrapeError::ServerTypeMismatch(format!(
                    "{} is a Stratum1 server, but no replicas were found in the repositories.json",
                    self.hostname.0
                )),
            )),
            (ServerType::SyncServer, true) => Err(CVMFSScraperError::ScrapeError(
                ScrapeError::ServerTypeMismatch(format!(
                    "{} is a SyncServer, but no replicas were found in the repositories.json",
                    self.hostname.0
                )),
            )),
            _ => Ok(()),
        }
    }

    fn merge_metadata(
        &self,
        repo_meta: MetadataFromRepoJSON,
        meta_json: Option<MetaJSON>,
    ) -> ServerMetadata {
        let mut server_metadata = if let Some(meta) = meta_json {
            ServerMetadata::from(meta)
        } else {
            ServerMetadata {
                schema_version: None,
                cvmfs_version: None,
                last_geodb_update: MaybeRfc2822DateTime(None),
                os_version_id: None,
                os_pretty_name: None,
                os_id: None,
                administrator: None,
                email: None,
                organisation: None,
                custom: None,
            }
        };

        server_metadata.merge_repo_metadata(repo_meta);
        server_metadata
    }
}

impl std::fmt::Display for PopulatedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({:?}, {:?})",
            self.hostname.0, self.server_type, self.backend_type
        )
    }
}

impl PopulatedServer {
    pub fn display(&self) {
        println!("Server: {}", self.hostname);
        println!("Type: {:?}", self.server_type);
        println!("Backend: {:?}", self.backend_type);
        if self.backend_type == ServerBackendType::AutoDetect {
            println!("Detected Backend: {:?}", self.backend_detected);
        }
        if self.backend_detected != ServerBackendType::S3 {
            self.metadata.display();
        } else {
            println!("Metadata: Not vailable for S3 servers.");
        }
        println!("Repositories:");
        for repo in &self.repositories {
            println!("  {}", repo.name);
            repo.display();
        }
    }

    pub fn has_repository(&self, repository: &str) -> bool {
        self.repositories.iter().any(|r| r.name == *repository)
    }
}

/// Metadata about the server from the repositories.json file.
///
/// Note that all the fields are optional. They are not set if the backend is S3, and a CVMFS server
/// may opt not to provide some of the fields for privacy reasons.
///
/// - schema_version: The schema version, typically 1
/// - cvmfs_version: The version of CVMFS running on the server
/// - last_geodb_update: The last time the GeoDB was updated
/// - os_version_id: The version of the operating system
/// - os_pretty_name: The pretty name of the operating system
/// - os_id: The ID of the operating system (e.g. rhel)
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataFromRepoJSON {
    pub schema_version: Option<u32>,
    pub cvmfs_version: Option<semver::Version>,
    pub last_geodb_update: MaybeRfc2822DateTime,
    pub os_version_id: Option<String>,
    pub os_pretty_name: Option<String>,
    pub os_id: Option<String>,
}

impl TryFrom<RepositoriesJSON> for MetadataFromRepoJSON {
    type Error = ScrapeError;

    fn try_from(repo_json: RepositoriesJSON) -> Result<Self, Self::Error> {
        let cvmfs_version = repo_json
            .cvmfs_version
            .clone()
            .map(|v| {
                v.parse::<semver::Version>()
                    .map_err(|e| ScrapeError::ConversionError(e.to_string()))
            })
            .transpose()?;

        Ok(MetadataFromRepoJSON {
            schema_version: Some(repo_json.schema),
            cvmfs_version,
            last_geodb_update: repo_json.last_geodb_update.clone(),
            os_version_id: repo_json.os_version_id.clone(),
            os_pretty_name: repo_json.os_pretty_name.clone(),
            os_id: repo_json.os_id.clone(),
        })
    }
}

/// Merged metadata about the server from the repositories.json and meta.json files.
///
/// This struct contains metadata about the server. It is a combination of the metadata from the
/// repositories.json file and the meta.json file.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerMetadata {
    pub schema_version: Option<u32>,
    pub cvmfs_version: Option<semver::Version>,
    pub last_geodb_update: MaybeRfc2822DateTime,
    pub os_version_id: Option<String>,
    pub os_pretty_name: Option<String>,
    pub os_id: Option<String>,
    pub administrator: Option<String>,
    pub email: Option<String>,
    pub organisation: Option<String>,
    pub custom: Option<serde_json::Value>,
}

impl From<MetaJSON> for ServerMetadata {
    fn from(meta: MetaJSON) -> Self {
        ServerMetadata {
            schema_version: None,
            cvmfs_version: None,
            last_geodb_update: MaybeRfc2822DateTime(None),
            os_version_id: None,
            os_pretty_name: None,
            os_id: None,
            administrator: Some(meta.administrator),
            email: Some(meta.email),
            organisation: Some(meta.organisation),
            custom: Some(meta.custom),
        }
    }
}

impl ServerMetadata {
    pub fn merge_repo_metadata(&mut self, repo_meta: MetadataFromRepoJSON) {
        self.schema_version = repo_meta.schema_version;
        self.cvmfs_version = repo_meta.cvmfs_version;
        self.last_geodb_update = repo_meta.last_geodb_update;
        self.os_version_id = repo_meta.os_version_id;
        self.os_pretty_name = repo_meta.os_pretty_name;
        self.os_id = repo_meta.os_id;
    }

    pub fn display(&self) {
        println!("Metadata:");
        if let Some(schema_version) = self.schema_version {
            println!("  Schema Version: {}", schema_version);
        }
        if let Some(cvmfs_version) = &self.cvmfs_version {
            println!("  CVMFS Version: {}", cvmfs_version);
        }
        if let MaybeRfc2822DateTime(Some(last_geodb_update)) = &self.last_geodb_update {
            println!("  Last GeoDB Update: {}", last_geodb_update);
        }
        if let Some(os_version_id) = &self.os_version_id {
            println!("  OS Version ID: {}", os_version_id);
        }
        if let Some(os_pretty_name) = &self.os_pretty_name {
            println!("  OS Pretty Name: {}", os_pretty_name);
        }
        if let Some(os_id) = &self.os_id {
            println!("  OS ID: {}", os_id);
        }
        if let Some(administrator) = &self.administrator {
            println!("  Administrator: {}", administrator);
        }
        if let Some(email) = &self.email {
            println!("  Email: {}", email);
        }
        if let Some(organisation) = &self.organisation {
            println!("  Organisation: {}", organisation);
        }
        if let Some(custom) = &self.custom {
            println!("  Custom: {}", custom);
        }
    }
}

pub struct RepositoryOrReplica {
    pub server: Server,
    pub name: String,
}

impl RepositoryOrReplica {
    pub fn new(name: &str, server: &Server) -> Self {
        RepositoryOrReplica {
            server: server.clone(),
            name: name.to_string(),
        }
    }

    pub async fn scrape(
        &self,
        client: &reqwest::Client,
    ) -> Result<PopulatedRepositoryOrReplica, CVMFSScraperError> {
        let repo_status = self.fetch_repository_status_json(client).await?;
        Ok(PopulatedRepositoryOrReplica {
            name: self.name.clone(),
            manifest: self.fetch_repository_manifest(client).await?,
            last_snapshot: repo_status.last_snapshot,
            last_gc: repo_status.last_gc,
        })
    }

    async fn fetch_repository_manifest(
        &self,
        client: &reqwest::Client,
    ) -> Result<Manifest, ManifestError> {
        let url = format!(
            "http://{}/cvmfs/{}/.cvmfspublished",
            self.server.hostname.0, self.name
        );
        let response = client.get(url).send().await?;
        let content = response.error_for_status()?.text().await?;
        let content = content.as_str();
        // println!("{}", content);
        Manifest::from_str(content)
    }

    async fn fetch_repository_status_json(
        &self,
        client: &reqwest::Client,
    ) -> Result<StatusJSON, ScrapeError> {
        fetch_json(
            client,
            format!(
                "http://{}/cvmfs/{}/.cvmfs_status.json",
                self.server.hostname.0, self.name
            ),
        )
        .await
    }
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct PopulatedRepositoryOrReplica {
    pub name: String,
    pub manifest: Manifest,
    pub last_snapshot: MaybeRfc2822DateTime,
    pub last_gc: MaybeRfc2822DateTime,
}

impl PopulatedRepositoryOrReplica {
    pub fn display(&self) {
        println!(" Name: {}", self.name);
        println!("  Last Snapshot: {}", self.last_snapshot);
        println!("  Last GC: {}", self.last_gc);
        self.manifest.display();
    }
    pub fn revision(&self) -> i32 {
        self.manifest.s
    }
}
