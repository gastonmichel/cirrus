//! Minimal clients for the Cloud Resource Manager and Cloud SQL Admin REST APIs.

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::gcloud;
use crate::model::{Instance, Project};

const CRM: &str = "https://cloudresourcemanager.googleapis.com/v1";
const SQLADMIN: &str = "https://sqladmin.googleapis.com/v1";

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("cirrus/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client")
    })
}

/// Sends an authenticated request, retrying once with a fresh token on 401.
async fn call<T: DeserializeOwned>(method: Method, url: &str, query: &[(&str, &str)], body: Option<&Value>) -> Result<T> {
    for attempt in 0..2 {
        let token = gcloud::access_token().await?;
        let mut req = http().request(method.clone(), url).bearer_auth(&token).query(query);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await.with_context(|| format!("request to {url} failed"))?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED && attempt == 0 {
            gcloud::invalidate_token();
            continue;
        }
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("{}", google_error_message(&text).unwrap_or_else(|| format!("HTTP {status}: {text}")));
        }
        return serde_json::from_str(&text).with_context(|| format!("unexpected response from {url}"));
    }
    unreachable!()
}

fn google_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")?.as_str().map(str::to_string)
}

pub async fn list_projects() -> Result<Vec<Project>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Page {
        #[serde(default)]
        projects: Vec<Project>,
        next_page_token: Option<String>,
    }
    let url = format!("{CRM}/projects");
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let mut query = vec![("pageSize", "500"), ("filter", "lifecycleState:ACTIVE")];
        if !token.is_empty() {
            query.push(("pageToken", &token));
        }
        let page: Page = call(Method::GET, &url, &query, None).await?;
        out.extend(page.projects);
        match page.next_page_token.filter(|t| !t.is_empty()) {
            Some(t) => token = t,
            None => break,
        }
    }
    out.sort_by(|a, b| a.project_id.cmp(&b.project_id));
    Ok(out)
}

pub async fn list_instances(project: &str) -> Result<Vec<Instance>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Page {
        #[serde(default)]
        items: Vec<Instance>,
        next_page_token: Option<String>,
    }
    let url = format!("{SQLADMIN}/projects/{project}/instances");
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let query: Vec<(&str, &str)> = if token.is_empty() { vec![] } else { vec![("pageToken", &token)] };
        let page: Page = call(Method::GET, &url, &query, None).await?;
        out.extend(page.items);
        match page.next_page_token.filter(|t| !t.is_empty()) {
            Some(t) => token = t,
            None => break,
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub async fn list_databases(project: &str, instance: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Db {
        name: String,
    }
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        items: Vec<Db>,
    }
    let url = format!("{SQLADMIN}/projects/{project}/instances/{instance}/databases");
    let page: Page = call(Method::GET, &url, &[], None).await?;
    let mut names: Vec<String> = page.items.into_iter().map(|d| d.name).collect();
    names.sort();
    Ok(names)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectSettings {
    pub server_ca_cert: SslCert,
    #[serde(default)]
    pub ip_addresses: Vec<IpMapping>,
    #[serde(default)]
    pub database_version: Option<String>,
    #[serde(default)]
    pub server_ca_mode: Option<String>,
    #[serde(default)]
    pub dns_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SslCert {
    pub cert: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpMapping {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    pub ip_address: String,
}

impl ConnectSettings {
    /// Prefer the public IP; fall back to a private one (requires VPC access).
    pub fn preferred_ip(&self) -> Result<&str> {
        let by_kind = |k: &str| self.ip_addresses.iter().find(|ip| ip.kind.as_deref() == Some(k));
        by_kind("PRIMARY")
            .or_else(|| by_kind("PRIVATE"))
            .map(|ip| ip.ip_address.as_str())
            .ok_or_else(|| anyhow!("instance has no public or private IP address"))
    }

    /// Instances using Certificate Authority Service CAs are verified by DNS name.
    pub fn uses_cas(&self) -> bool {
        matches!(
            self.server_ca_mode.as_deref(),
            Some("GOOGLE_MANAGED_CAS_CA" | "CUSTOMER_MANAGED_CAS_CA")
        )
    }
}

pub async fn connect_settings(project: &str, instance: &str) -> Result<ConnectSettings> {
    let url = format!("{SQLADMIN}/projects/{project}/instances/{instance}/connectSettings");
    call(Method::GET, &url, &[], None).await
}

/// Requests a short-lived client certificate signed by the instance's CA. When
/// `access_token` is given the certificate is bound to that IAM identity (IAM DB auth).
pub async fn generate_ephemeral_cert(
    project: &str,
    instance: &str,
    public_key_pem: &str,
    access_token: &str,
) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resp {
        ephemeral_cert: SslCert,
    }
    let url = format!("{SQLADMIN}/projects/{project}/instances/{instance}:generateEphemeralCert");
    let body = serde_json::json!({ "public_key": public_key_pem, "access_token": access_token });
    let resp: Resp = call(Method::POST, &url, &[], Some(&body)).await?;
    Ok(resp.ephemeral_cert.cert)
}
