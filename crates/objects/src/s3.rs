use crate::{Error, MAX_BYTES, ObjectRef, Result, VerifiedObject, identifier, sha256};
use agent_computer_storage::quota::read_private;
use hmac::{Hmac, Mac};
use reqwest::{Method, StatusCode, Url, header::HeaderMap};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub credentials_file: PathBuf,
    pub ca_file: Option<PathBuf>,
    #[serde(default)]
    pub allow_http: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    access_key: String,
    secret_key: String,
}

/// Explicit credentials and endpoint only; no redirects, ambient proxy or retry.
pub struct Client {
    http: reqwest::Client,
    endpoint: Url,
    region: String,
    bucket: String,
    credentials: Credentials,
    store_digest: String,
}
impl Client {
    pub fn new(config: &Configuration) -> Result<Self> {
        let endpoint = Url::parse(&config.endpoint).map_err(|_| Error::Configuration)?;
        if !(endpoint.scheme() == "https" || config.allow_http && endpoint.scheme() == "http")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.path() != "/"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !identifier(&config.region)
            || config.bucket.len() < 3
            || config.bucket.len() > 63
            || !config
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || !config.bucket.as_bytes()[0].is_ascii_alphanumeric()
            || !config.bucket.as_bytes()[config.bucket.len() - 1].is_ascii_alphanumeric()
        {
            return Err(Error::Configuration);
        }
        let credentials: Credentials = serde_json::from_slice(
            &read_private(&config.credentials_file, 8192).map_err(|_| Error::Configuration)?,
        )
        .map_err(|_| Error::Configuration)?;
        if credentials.access_key.is_empty()
            || credentials.access_key.len() > 128
            || !credentials
                .access_key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || credentials.secret_key.len() < 16
            || credentials.secret_key.len() > 256
            || !credentials.secret_key.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Error::Configuration);
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10));
        if let Some(path) = &config.ca_file {
            let bytes = read_private(path, 65536).map_err(|_| Error::Configuration)?;
            let cert = reqwest::Certificate::from_pem(&bytes).map_err(|_| Error::Configuration)?;
            builder = builder.tls_certs_only([cert]);
        }
        let http = builder.build().map_err(|_| Error::Configuration)?;
        let store_digest = sha256(
            format!(
                "agent-computer/s3-store-v1\0{}\0{}\0{}",
                endpoint, config.region, config.bucket
            )
            .as_bytes(),
        );
        Ok(Self {
            http,
            endpoint,
            region: config.region.clone(),
            bucket: config.bucket.clone(),
            credentials,
            store_digest,
        })
    }
    pub fn store_digest(&self) -> &str {
        &self.store_digest
    }
    pub fn reference(
        &self,
        organization: &str,
        execution: &str,
        bytes: &[u8],
    ) -> Result<ObjectRef> {
        if !identifier(organization) || !identifier(execution) || bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        let hash = sha256(bytes);
        Ok(ObjectRef {
            store_digest: self.store_digest.clone(),
            key: format!(
                "execution-outputs/v1/{organization}/{execution}/{}",
                &hash[7..]
            ),
            sha256: hash,
            size: bytes.len() as u64,
        })
    }
    fn bound(&self, object: &ObjectRef) -> Result<Url> {
        object.validate()?;
        if object.store_digest != self.store_digest {
            return Err(Error::Integrity);
        }
        self.endpoint
            .join(&format!("{}/{}", self.bucket, object.key))
            .map_err(|_| Error::Invalid)
    }
    fn request(
        &self,
        method: Method,
        object: &ObjectRef,
        body: &[u8],
        conditional: bool,
    ) -> Result<reqwest::RequestBuilder> {
        let url = self.bound(object)?;
        let time = time::OffsetDateTime::now_utc();
        let format = time::format_description::parse_borrowed::<2>(
            "[year][month][day]T[hour][minute][second]Z",
        )
        .map_err(|_| Error::Configuration)?;
        let timestamp = time.format(&format).map_err(|_| Error::Configuration)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "host",
            url[url::Position::BeforeHost..url::Position::AfterPort]
                .parse()
                .map_err(|_| Error::Invalid)?,
        );
        headers.insert("x-amz-date", timestamp.parse().map_err(|_| Error::Invalid)?);
        let hash = sha256(body);
        headers.insert(
            "x-amz-content-sha256",
            hash[7..].parse().map_err(|_| Error::Invalid)?,
        );
        if conditional {
            headers.insert("if-none-match", "*".parse().unwrap());
        }
        let authorization = sign(
            method.as_str(),
            url.path(),
            &headers,
            &timestamp,
            &self.region,
            &self.credentials,
        )?;
        let mut authorization: reqwest::header::HeaderValue =
            authorization.parse().map_err(|_| Error::Configuration)?;
        authorization.set_sensitive(true);
        headers.insert("authorization", authorization);
        Ok(self.http.request(method, url).headers(headers))
    }
    pub async fn get(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        let mut response = self
            .request(Method::GET, object, &[], false)?
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(Error::Missing);
        }
        if response.status() != StatusCode::OK {
            return Err(Error::Rejected);
        }
        if response.content_length().is_some_and(|n| n != object.size) {
            return Err(Error::Integrity);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if chunk.len() > (object.size as usize).saturating_sub(bytes.len()) {
                return Err(Error::Limit);
            }
            bytes.extend_from_slice(&chunk);
        }
        object.verify(&bytes)?;
        Ok(bytes)
    }
    pub async fn put_verified(&self, object: &ObjectRef, bytes: &[u8]) -> Result<VerifiedObject> {
        object.verify(bytes)?;
        let response = self
            .request(Method::PUT, object, bytes, true)?
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if !matches!(
            response.status(),
            StatusCode::OK | StatusCode::PRECONDITION_FAILED
        ) {
            return Err(Error::Rejected);
        }
        // ETag is not a content digest and a successful PUT is not read-back proof.
        if self.get(object).await? != bytes {
            return Err(Error::Integrity);
        }
        Ok(VerifiedObject(object.clone()))
    }
}

fn mac(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut h = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    h.update(value);
    h.finalize().into_bytes().to_vec()
}
fn sign(
    method: &str,
    path: &str,
    headers: &HeaderMap,
    timestamp: &str,
    region: &str,
    credentials: &Credentials,
) -> Result<String> {
    let mut canonical = std::collections::BTreeMap::new();
    for (key, value) in headers {
        canonical.insert(key.as_str(), value.to_str().map_err(|_| Error::Invalid)?);
    }
    let signed = canonical.keys().copied().collect::<Vec<_>>().join(";");
    let fields = canonical
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let request = format!(
        "{method}\n{path}\n\n{fields}\n{signed}\n{}",
        canonical["x-amz-content-sha256"]
    );
    let date = &timestamp[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{:x}",
        Sha256::digest(request.as_bytes())
    );
    let key = mac(
        format!("AWS4{}", credentials.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let key = mac(&key, region.as_bytes());
    let key = mac(&key, b"s3");
    let key = mac(&key, b"aws4_request");
    let signature = mac(&key, string.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        credentials.access_key
    ))
}

#[cfg(test)]
mod tests;
