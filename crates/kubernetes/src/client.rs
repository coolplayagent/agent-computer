use crate::{
    EphemeralSandboxPlan, Error, Result,
    plan::{dns_label, opaque},
    verify,
};
use reqwest::{
    Method, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const RESPONSE_LIMIT: usize = 1024 * 1024;

/// Trusted deployment bindings, provisioned by an operator outside the tenant API.
/// Cluster RBAC must reserve the namespace/policy to the operator and this controller.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub namespace: String,
    pub namespace_uid: String,
    pub runtime_class_uid: String,
    pub deny_policy_uid: String,
    pub network_policy_ref: String,
}

pub struct Client {
    pub(crate) http: reqwest::Client,
    pub(crate) endpoint: Url,
    pub(crate) deployment: Deployment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PodPhase {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
    Deleting,
}

/// API observations only. Running does not establish application readiness; neither
/// Succeeded/Failed nor an absent object is a node/process fencing certificate.
#[derive(Clone, Debug)]
pub struct PodObservation {
    pub(crate) uid: String,
    pub(crate) resource_version: String,
    pub(crate) binding: String,
    pub(crate) phase: PodPhase,
}

impl PodObservation {
    pub fn uid(&self) -> &str {
        &self.uid
    }
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }
    pub fn phase(&self) -> PodPhase {
        self.phase
    }
}

/// Both outcomes concern API object deletion, never physical process termination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteOutcome {
    Requested,
    AlreadyAbsent,
}

impl Client {
    pub fn namespace(&self) -> &str {
        &self.deployment.namespace
    }
    pub fn namespace_uid(&self) -> &str {
        &self.deployment.namespace_uid
    }
    /// No kubeconfig discovery, credential helper execution, ambient proxy, redirect,
    /// system trust roots, or automatic HTTP retry. Only the explicit CA is trusted.
    pub fn new(
        endpoint: &str,
        ca_pem: &[u8],
        bearer: &str,
        deployment: Deployment,
    ) -> Result<Self> {
        let endpoint = Url::parse(endpoint).map_err(|_| Error::InvalidConfiguration)?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.path() != "/"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !dns_label(&deployment.namespace)
            || ![
                &deployment.namespace_uid,
                &deployment.runtime_class_uid,
                &deployment.deny_policy_uid,
            ]
            .into_iter()
            .all(|s| opaque(s))
            || deployment.network_policy_ref.is_empty()
            || deployment.network_policy_ref.len() > 132
            || bearer.is_empty()
            || bearer.len() > 16384
            || !bearer
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
            || ca_pem.is_empty()
            || ca_pem.len() > 65536
        {
            return Err(Error::InvalidConfiguration);
        }
        let mut roots = rustls::RootCertStore::empty();
        for der in CertificateDer::pem_slice_iter(ca_pem) {
            roots
                .add(der.map_err(|_| Error::InvalidConfiguration)?)
                .map_err(|_| Error::InvalidConfiguration)?;
        }
        if roots.is_empty() {
            return Err(Error::InvalidConfiguration);
        }
        let tls = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::InvalidConfiguration)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {bearer}"))
            .map_err(|_| Error::InvalidConfiguration)?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .default_headers(headers)
            .build()
            .map_err(|_| Error::InvalidConfiguration)?;
        Ok(Self {
            http,
            endpoint,
            deployment,
        })
    }

    /// Check actual object UIDs and the narrow deployment policy before creating a Pod.
    /// A RuntimeClass declaration is not proof that a node is executing runsc.
    pub async fn probe(&self) -> Result<()> {
        let d = &self.deployment;
        let namespace = self
            .get(&format!("/api/v1/namespaces/{}", d.namespace))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        let runtime = self
            .get("/apis/node.k8s.io/v1/runtimeclasses/gvisor")
            .await?
            .ok_or(Error::PreconditionFailed)?;
        let policies = self
            .get(&format!(
                "/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies",
                d.namespace
            ))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        verify::deployment(d, &namespace, &runtime, &policies)
    }

    /// Exactly one POST; no get-or-create, retry or replacement. On ExistingObject or
    /// MutationUnconfirmed, persist uncertainty and observe this same instance identity.
    pub async fn create(&self, plan: &EphemeralSandboxPlan) -> Result<PodObservation> {
        self.check_plan(plan)?;
        self.probe().await?;
        let response = self
            .request(Method::POST, &self.pod_path(plan, false), Some(&plan.pod))
            .await;
        match response {
            Ok((409, _)) => Err(Error::ExistingObject),
            Ok((201, pod)) => verify::pod(plan, &pod, None).map_err(|_| Error::MutationUnconfirmed),
            _ => Err(Error::MutationUnconfirmed),
        }
    }

    /// Read back identity AND admitted fields. Pass the recorded UID once it is known.
    /// With no UID (lost create response), adoption requires all persisted bindings to match.
    pub async fn observe(
        &self,
        plan: &EphemeralSandboxPlan,
        uid: Option<&str>,
    ) -> Result<Option<PodObservation>> {
        self.check_plan(plan)?;
        self.probe().await?;
        self.get(&self.pod_path(plan, true))
            .await?
            .map(|pod| verify::pod(plan, &pod, uid))
            .transpose()
    }

    /// Conditional deletion of the exact observed object version. A 404 is only absence
    /// from the API; do not release a Workspace writer or increment generation from it.
    pub async fn delete(
        &self,
        plan: &EphemeralSandboxPlan,
        observed: &PodObservation,
    ) -> Result<DeleteOutcome> {
        self.check_plan(plan)?;
        if observed.binding != plan.binding {
            return Err(Error::IdentityMismatch);
        }
        let body = json!({"apiVersion": "v1", "kind": "DeleteOptions",
            "gracePeriodSeconds": 30, "propagationPolicy": "Foreground",
            "preconditions": {"uid": observed.uid, "resourceVersion": observed.resource_version}});
        match self
            .request(Method::DELETE, &self.pod_path(plan, true), Some(&body))
            .await
        {
            Ok((200 | 202, _)) => Ok(DeleteOutcome::Requested),
            Ok((404, _)) => Ok(DeleteOutcome::AlreadyAbsent),
            Ok((409, _)) => Err(Error::PreconditionFailed),
            _ => Err(Error::MutationUnconfirmed),
        }
    }

    fn check_plan(&self, plan: &EphemeralSandboxPlan) -> Result<()> {
        if plan.namespace != self.deployment.namespace
            || plan.network_policy_ref != self.deployment.network_policy_ref
        {
            return Err(Error::PreconditionFailed);
        }
        Ok(())
    }

    fn pod_path(&self, plan: &EphemeralSandboxPlan, named: bool) -> String {
        let collection = format!("/api/v1/namespaces/{}/pods", plan.namespace);
        if named {
            format!("{collection}/{}", plan.name)
        } else {
            collection
        }
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Option<Value>> {
        match self.request(Method::GET, path, None).await? {
            (200, value) => Ok(Some(value)),
            (404, _) => Ok(None),
            _ => Err(Error::ApiRejected),
        }
    }

    pub(crate) async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let url = self
            .endpoint
            .join(path)
            .map_err(|_| Error::InvalidConfiguration)?;
        let mut request = self.http.request(method, url);
        if let Some(body) = body {
            request = request.json(body);
        }
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        match status {
            401 | 403 => return Err(Error::AccessDenied),
            404 | 409 => return Ok((status, Value::Null)),
            200..=202 => (),
            _ => return Err(Error::ApiRejected),
        }
        if response
            .content_length()
            .is_some_and(|n| n > RESPONSE_LIMIT as u64)
        {
            return Err(Error::ResponseLimit);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if chunk.len() > RESPONSE_LIMIT - bytes.len() {
                return Err(Error::ResponseLimit);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
        Ok((status, value))
    }
}
