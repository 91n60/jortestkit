mod release;

use crate::web::{download_file, WebError};
use os_info::Type as OsType;
pub use release::{AssetDto, ReleaseDto};
use reqwest::header::USER_AGENT;
use semver::Version;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;
use thiserror::Error;

use sha1::Digest as _;

#[derive(Debug, Error)]
pub enum GitHubApiError {
    #[error("could not deserialize response")]
    CannotDeserialize(#[from] serde_json::Error),
    #[error("could not send reqeuest")]
    RequestError(#[from] reqwest::Error),
    #[error("cannot find release with version: {0}")]
    CannotFindReleaseWithVersion(String),
    #[error("API rate limit exceeded")]
    RateLimitExceeded,
    #[error("checksum failed")]
    WrongChecksum,
    #[error("invalid checksum")]
    InvalidChecksum(#[from] hex::FromHexError),
    #[error(transparent)]
    Download(#[from] WebError),
    #[error(transparent)]
    IO(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
enum ChecksumType {
    Sha256,
    Sha1,
}

impl ChecksumType {
    fn extension(&self) -> &'static str {
        match self {
            Self::Sha1 => ".sha1",
            Self::Sha256 => ".sha256",
        }
    }

    fn from_filename(filename: &Path) -> Option<Self> {
        let extension = filename.extension()?.to_str()?;
        match extension {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }

    fn check(&self, checksum: &[u8], file: &Path) -> Result<bool, GitHubApiError> {
        let contents = std::fs::read(file)?;
        match self {
            ChecksumType::Sha256 => Ok(sha2::Sha256::digest(&contents).as_slice() == checksum),
            ChecksumType::Sha1 => Ok(sha1::Sha1::digest(&contents).as_slice() == checksum),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Asset {
    asset: AssetDto,
    checksum: Option<(ChecksumType, AssetDto)>,
}

impl Asset {
    pub fn assets_from_dtos(dtos: Vec<AssetDto>) -> Vec<Self> {
        let mut assets = Vec::new();
        let mut checksums = HashMap::new();
        for dto in dtos {
            let asset_name = dto.name();
            match ChecksumType::from_filename(Path::new(&asset_name)) {
                Some(checksum_type) => {
                    let name = asset_name
                        .strip_suffix(checksum_type.extension())
                        .unwrap()
                        .to_owned();
                    checksums.insert(name, (checksum_type, dto));
                }
                None => assets.push(dto),
            }
        }
        let mut res = Vec::new();
        for asset in assets {
            res.push(Asset {
                checksum: checksums.remove(&asset.name()),
                asset,
            });
        }
        res
    }

    pub fn download_to(&self, path: &Path) -> Result<(), GitHubApiError> {
        download_file(self.download_url(), path)?;
        if let Some((checksum_type, asset)) = &self.checksum {
            let checksum = reqwest::blocking::get(&asset.download_url())?;
            if !checksum_type.check(&hex::decode(checksum.text()?)?, path)? {
                return Err(GitHubApiError::WrongChecksum);
            }
        }
        Ok(())
    }

    pub fn os_type(&self) -> OsType {
        self.asset.os_type()
    }

    pub fn download_url(&self) -> String {
        self.asset.download_url()
    }

    pub fn name(&self) -> String {
        self.asset.name()
    }
}

#[derive(Clone, Debug)]
pub struct Release {
    version: String,
    released_date: SystemTime,
    releases_per_os: HashMap<OsType, Asset>,
    prerelease: bool,
}

impl Release {
    pub fn get_release_for_os(&self, os_type: OsType) -> Option<Asset> {
        let compacted_os_type = self.compact_os_types(os_type);
        self.releases_per_os().get(&compacted_os_type).cloned()
    }

    pub fn assets(&self) -> Vec<Asset> {
        self.releases_per_os.values().cloned().collect()
    }

    /// narrow linux distribution to linux type
    #[allow(clippy::all)]
    fn compact_os_types(&self, os_type: OsType) -> OsType {
        match os_type {
            OsType::Android => OsType::Android,
            OsType::Macos => OsType::Macos,
            OsType::Redox => OsType::Redox,
            OsType::Unknown => OsType::Unknown,
            OsType::Windows => OsType::Windows,
            _ => OsType::Linux,
        }
    }

    pub fn releases_per_os(&self) -> &HashMap<OsType, Asset> {
        &self.releases_per_os
    }

    pub fn version_str(&self) -> String {
        self.version.clone()
    }

    pub fn version(&self) -> Version {
        Version::parse(Self::without_first(&self.version_str())).unwrap()
    }

    fn without_first(string: &str) -> &str {
        string
            .char_indices()
            .nth(1)
            .and_then(|(i, _)| string.get(i..))
            .unwrap_or("")
    }

    pub fn prerelease(&self) -> bool {
        self.prerelease
    }
}

pub struct CachedReleases {
    inner: Vec<Release>,
}

impl CachedReleases {
    pub fn new(inner: Vec<Release>) -> Self {
        Self { inner }
    }

    pub fn get_asset_for_current_os_by_version(
        &self,
        version: String,
    ) -> Result<Option<Asset>, GitHubApiError> {
        let info = os_info::get();
        match self.inner.iter().find(|x| *x.version == version) {
            None => Err(GitHubApiError::CannotFindReleaseWithVersion(version)),
            Some(release) => Ok(release.get_release_for_os(info.os_type())),
        }
    }
}

impl<'a> IntoIterator for &'a CachedReleases {
    type Item = &'a Release;
    type IntoIter = std::slice::Iter<'a, Release>;

    fn into_iter(self) -> std::slice::Iter<'a, Release> {
        self.inner.iter()
    }
}

pub struct GitHubApi {
    base_url: String,
}

impl Default for GitHubApi {
    fn default() -> Self {
        Self::new()
    }
}

impl GitHubApi {
    pub fn for_crate<S: Into<String>>(base_url: S) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    pub fn new() -> Self {
        Self {
            base_url: "https://api.github.com/repos/input-output-hk/jormungandr".to_string(),
        }
    }

    fn get(&self, path: &str) -> Result<reqwest::blocking::Response, GitHubApiError> {
        let client = reqwest::blocking::Client::new();
        let resp = client
            .get(&format!("{}/{}", self.base_url, path))
            .header(USER_AGENT, "request")
            .send()
            .map_err(GitHubApiError::RequestError)?;
        if resp.headers().get("X-RateLimit-Remaining") == Some(0.into()).as_ref() {
            return Err(GitHubApiError::RateLimitExceeded);
        }

        Ok(resp)
    }

    pub fn describe_releases(&self) -> Result<CachedReleases, GitHubApiError> {
        let response_text = self.get("releases")?.text()?;
        let releases: Vec<ReleaseDto> =
            serde_json::from_str(&response_text).map_err(GitHubApiError::CannotDeserialize)?;
        Ok(CachedReleases::new(
            releases
                .iter()
                .cloned()
                .map(|release| release.into())
                .collect(),
        ))
    }
}
