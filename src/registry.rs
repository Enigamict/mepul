use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use surf::middleware::{Middleware, Next};
use surf::{Client, Request, Response, StatusCode};

use crate::image_ref::ImageReference;
use crate::types::{Descriptor, ImageIndex, ImageManifest};

const ACCEPTED_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
struct UserAgentMiddleware;

#[surf::utils::async_trait]
impl Middleware for UserAgentMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        client: Client,
        next: Next<'_>,
    ) -> surf::Result<Response> {
        req.insert_header("User-Agent", USER_AGENT);
        next.run(req, client).await
    }
}

#[derive(Clone)]
pub struct RegistryClient {
    client: Client,
}

impl RegistryClient {
    pub fn new() -> Result<Self> {
        let client = Client::new().with(UserAgentMiddleware);
        Ok(Self { client })
    }

    pub async fn pull(&self, image: &ImageReference, platform: &PlatformSpec) -> Result<PullPlan> {
        let tag_manifest = self.fetch_manifest_bytes(image, &image.reference).await?;
        let (manifest_descriptor, manifest) =
            self.resolve_manifest(image, tag_manifest, platform).await?;
        Ok(PullPlan {
            manifest_descriptor,
            manifest,
        })
    }

    pub async fn fetch_blob(&self, image: &ImageReference, digest: &str) -> Result<Vec<u8>> {
        let url = format!(
            "https://{}/v2/{}/blobs/{}",
            image.registry, image.repository, digest
        );
        let mut response = self
            .get_with_auth(&url, Scope::repository_pull(&image.repository))
            .await?;

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            bail!("HTTP Error: {}", status);
        }

        Ok(response
            .body_bytes()
            .await
            .or_else(|err| bail!("{}", err))?)
    }

    async fn resolve_manifest(
        &self,
        image: &ImageReference,
        candidate: ManifestBytes,
        platform: &PlatformSpec,
    ) -> Result<(Descriptor, ResolvedManifest)> {
        if is_manifest_list(&candidate.media_type) {
            let index: ImageIndex = serde_json::from_slice(&candidate.bytes)?;
            let selected = index
                .manifests
                .into_iter()
                .find(|m| m.platform.os == platform.os && m.platform.architecture == platform.arch)
                .ok_or_else(|| {
                    anyhow!("no manifest found for {}/{}", platform.os, platform.arch)
                })?;

            let manifest_bytes = self
                .fetch_manifest_bytes(image, &selected.descriptor.digest)
                .await?;
            let manifest = decode_manifest(manifest_bytes)?;
            Ok((selected.descriptor, manifest))
        } else {
            let descriptor = Descriptor {
                media_type: candidate.media_type.clone(),
                digest: candidate.digest.clone(),
                size: candidate.bytes.len() as u64,
            };
            let manifest = decode_manifest(candidate)?;
            Ok((descriptor, manifest))
        }
    }

    async fn fetch_manifest_bytes(
        &self,
        image: &ImageReference,
        reference: &str,
    ) -> Result<ManifestBytes> {
        let url = format!(
            "https://{}/v2/{}/manifests/{}",
            image.registry, image.repository, reference
        );

        let mut response = self
            .get_with_auth_and_headers(
                &url,
                Scope::repository_pull(&image.repository),
                vec![("Accept", ACCEPTED_MANIFESTS)],
            )
            .await?;

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            bail!("HTTP Error: {}", status);
        }

        let digest = response
            .header("docker-content-digest")
            .map(|vals| vals.last().as_str().to_string())
            .ok_or_else(|| anyhow!("missing Docker-Content-Digest header"))?;
        let media_type = response
            .header("content-type")
            .map(|vals| {
                vals.last()
                    .as_str()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        let bytes = response
            .body_bytes()
            .await
            .or_else(|err| bail!("{}", err))?;

        Ok(ManifestBytes {
            digest,
            media_type,
            bytes,
        })
    }

    async fn get_with_auth(&self, url: &str, scope: Scope) -> Result<Response> {
        self.get_with_auth_and_headers(url, scope, vec![]).await
    }

    async fn get_with_auth_and_headers(
        &self,
        url: &str,
        scope: Scope,
        headers: Vec<(&str, &str)>,
    ) -> Result<Response> {
        let mut req = self.client.get(url);
        for (k, v) in &headers {
            req = req.header(*k, *v);
        }
        let initial = req.await.or_else(|err| bail!("{}", err))?;

        if initial.status() != StatusCode::Unauthorized {
            return Ok(initial);
        }

        let challenge_str = initial
            .header("WWW-Authenticate")
            .map(|vals| vals.last().as_str().to_string())
            .ok_or_else(|| anyhow!("missing WWW-Authenticate header"))?;
        let bearer = BearerChallenge::parse(&challenge_str)?;
        let token = self.fetch_bearer_token(&bearer, &scope).await?;

        let mut retried = self.client.get(url);
        for (k, v) in &headers {
            retried = retried.header(*k, *v);
        }
        retried = retried.header("Authorization", format!("Bearer {token}"));

        Ok(retried.await.or_else(|err| bail!("{}", err))?)
    }

    async fn fetch_bearer_token(
        &self,
        challenge: &BearerChallenge,
        scope: &Scope,
    ) -> Result<String> {
        let mut url =
            surf::Url::parse(&challenge.realm).map_err(|e| anyhow!("invalid realm URL: {}", e))?;
        url.query_pairs_mut()
            .append_pair("service", &challenge.service)
            .append_pair("scope", scope.as_str());

        if let Some(extra_scope) = challenge.scope.as_deref() {
            url.query_pairs_mut().append_pair("scope", extra_scope);
        }

        let mut response = self.client.get(url).await.or_else(|err| bail!("{}", err))?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            bail!("HTTP Error: {}", status);
        }

        let token: TokenResponse = response.body_json().await.or_else(|err| bail!("{}", err))?;

        token
            .token
            .or(token.access_token)
            .ok_or_else(|| anyhow!("token response did not contain a bearer token"))
    }
}

pub struct PullPlan {
    pub manifest_descriptor: Descriptor,
    pub manifest: ResolvedManifest,
}

pub struct ResolvedManifest {
    pub digest: String,
    pub raw_bytes: Vec<u8>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

pub struct PlatformSpec {
    pub os: String,
    pub arch: String,
}

impl PlatformSpec {
    pub fn host_default() -> Self {
        Self {
            os: "linux".to_string(),
            arch: normalize_arch(std::env::consts::ARCH).to_string(),
        }
    }
}

fn normalize_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

#[derive(Clone)]
struct Scope(String);

impl Scope {
    fn repository_pull(repository: &str) -> Self {
        Self(format!("repository:{repository}:pull"))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

struct ManifestBytes {
    digest: String,
    media_type: String,
    bytes: Vec<u8>,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

struct BearerChallenge {
    realm: String,
    service: String,
    scope: Option<String>,
}

impl BearerChallenge {
    fn parse(header: &str) -> Result<Self> {
        let Some(rest) = header.strip_prefix("Bearer ") else {
            bail!("unsupported auth challenge: {header}");
        };

        let mut realm = None;
        let mut service = None;
        let mut scope = None;

        for part in rest.split(',') {
            let mut kv = part.trim().splitn(2, '=');
            let key = kv.next().unwrap_or_default().trim();
            let value = kv
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('"')
                .to_string();

            match key {
                "realm" => realm = Some(value),
                "service" => service = Some(value),
                "scope" => scope = Some(value),
                _ => {}
            }
        }

        Ok(Self {
            realm: realm.ok_or_else(|| anyhow!("bearer challenge missing realm"))?,
            service: service.ok_or_else(|| anyhow!("bearer challenge missing service"))?,
            scope,
        })
    }
}

fn is_manifest_list(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.oci.image.index.v1+json"
            | "application/vnd.docker.distribution.manifest.list.v2+json"
    )
}

fn decode_manifest(candidate: ManifestBytes) -> Result<ResolvedManifest> {
    let manifest: ImageManifest = serde_json::from_slice(&candidate.bytes)?;
    Ok(ResolvedManifest {
        digest: candidate.digest,
        raw_bytes: candidate.bytes,
        config: manifest.config,
        layers: manifest.layers,
    })
}
