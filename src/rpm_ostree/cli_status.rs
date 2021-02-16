//! Interface to `rpm-ostree status --json`.

use super::actor::{RpmOstreeClient, StatusCache};
use super::Release;
use failure::{bail, ensure, format_err, Fallible, ResultExt};
use filetime::FileTime;
use log::trace;
use prometheus::IntCounter;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;

/// Path to local OSTree deployments. We use its mtime to check for modifications (e.g. new deployments)
/// to local deployments that might warrant querying `rpm-ostree status` again to update our knowledge
/// of the current state of deployments.
const OSTREE_DEPLS_PATH: &str = "/ostree/deploy";

lazy_static::lazy_static! {
    static ref STATUS_CACHE_ATTEMPTS: IntCounter = register_int_counter!(opts!(
        "zincati_rpm_ostree_status_cache_requests_total",
        "Total number of attempts to query rpm-ostree actor's cached status."
    )).unwrap();
    static ref STATUS_CACHE_MISSES: IntCounter = register_int_counter!(opts!(
        "zincati_rpm_ostree_status_cache_misses_total",
        "Total number of times rpm-ostree actor's cached status is stale during queries."
    )).unwrap();
    // This is not equivalent to `zincati_rpm_ostree_status_cache_misses_total` as there
    // are cases where `rpm-ostree status` is called directly without checking the cache.
    static ref RPM_OSTREE_STATUS_ATTEMPTS: IntCounter = register_int_counter!(opts!(
        "zincati_rpm_ostree_status_attempts_total",
        "Total number of 'rpm-ostree status' attempts."
    )).unwrap();
    static ref RPM_OSTREE_STATUS_FAILURES: IntCounter = register_int_counter!(opts!(
        "zincati_rpm_ostree_status_failures_total",
        "Total number of 'rpm-ostree status' failures."
    )).unwrap();
}

/// JSON output from `rpm-ostree status --json`
#[derive(Clone, Debug, Deserialize)]
pub struct StatusJSON {
    deployments: Vec<DeploymentJSON>,
}

/// Partial deployment object (only fields relevant to zincati).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DeploymentJSON {
    booted: bool,
    base_checksum: Option<String>,
    #[serde(rename = "base-commit-meta")]
    base_metadata: BaseCommitMetaJSON,
    checksum: String,
    // NOTE(lucab): missing field means "not staged".
    #[serde(default)]
    staged: bool,
    version: String,
}

/// Metadata from base commit (only fields relevant to zincati).
#[derive(Clone, Debug, Deserialize)]
struct BaseCommitMetaJSON {
    #[serde(rename = "coreos-assembler.basearch")]
    basearch: String,
    #[serde(rename = "fedora-coreos.stream")]
    stream: String,
}

impl DeploymentJSON {
    /// Convert into `Release`.
    pub fn into_release(self) -> Release {
        Release {
            checksum: self.base_revision(),
            version: self.version,
            age_index: None,
        }
    }

    /// Return the deployment base revision.
    pub fn base_revision(&self) -> String {
        self.base_checksum
            .clone()
            .unwrap_or_else(|| self.checksum.clone())
    }
}

/// Parse base architecture for booted deployment from status object.
pub fn parse_basearch(status: &StatusJSON) -> Fallible<String> {
    let json = booted_json(status)?;
    Ok(json.base_metadata.basearch)
}

/// Parse the booted deployment from status object.
pub fn parse_booted(status: &StatusJSON) -> Fallible<Release> {
    let json = booted_json(status)?;
    Ok(json.into_release())
}

/// Parse updates stream for booted deployment from status object.
pub fn parse_updates_stream(status: &StatusJSON) -> Fallible<String> {
    let json = booted_json(status)?;
    ensure!(!json.base_metadata.stream.is_empty(), "empty stream value");
    Ok(json.base_metadata.stream)
}

/// Parse local deployments from a status object.
fn parse_local_deployments(status: &StatusJSON, omit_staged: bool) -> Fallible<BTreeSet<Release>> {
    let mut deployments = BTreeSet::<Release>::new();
    for entry in &status.deployments {
        if omit_staged && entry.staged {
            continue;
        }

        let release = entry.clone().into_release();
        deployments.insert(release);
    }
    Ok(deployments)
}

/// Return local deployments, using client's cache if possible.
pub fn local_deployments(
    client: &mut RpmOstreeClient,
    omit_staged: bool,
) -> Fallible<BTreeSet<Release>> {
    let status = status_json(client)?;
    let local_depls = parse_local_deployments(&status, omit_staged)?;

    Ok(local_depls)
}

/// Return JSON object for booted deployment.
fn booted_json(status: &StatusJSON) -> Fallible<DeploymentJSON> {
    let booted = status
        .clone()
        .deployments
        .into_iter()
        .find(|d| d.booted)
        .ok_or_else(|| format_err!("no booted deployment found"))?;

    ensure!(!booted.base_revision().is_empty(), "empty base revision");
    ensure!(!booted.version.is_empty(), "empty version");
    ensure!(!booted.base_metadata.basearch.is_empty(), "empty basearch");
    Ok(booted)
}

/// Introspect deployments (rpm-ostree status) using rpm-ostree client actor client's
/// cache if possible.
fn status_json(client: &mut RpmOstreeClient) -> Fallible<StatusJSON> {
    STATUS_CACHE_ATTEMPTS.inc();
    let ostree_depls_data = fs::metadata(OSTREE_DEPLS_PATH)
        .with_context(|e| format_err!("failed to query directory {}: {}", OSTREE_DEPLS_PATH, e))?;
    let ostree_depls_data_mtime = FileTime::from_last_modification_time(&ostree_depls_data);

    if let Some(cache) = &client.status_cache {
        if cache.mtime == ostree_depls_data_mtime {
            trace!("cache fresh, using cached rpm-ostree status");
            return Ok(cache.status.clone());
        }
    }

    STATUS_CACHE_MISSES.inc();
    trace!("cache stale, invoking rpm-ostree to retrieve local deployments");
    let status = invoke_cli_status(false)?;
    client.status_cache = Some(StatusCache {
        status: status.clone(),
        mtime: ostree_depls_data_mtime,
    });

    Ok(status)
}

/// CLI executor for `rpm-ostree status --json`.
pub fn invoke_cli_status(booted_only: bool) -> Fallible<StatusJSON> {
    RPM_OSTREE_STATUS_ATTEMPTS.inc();

    let mut cmd = std::process::Command::new("rpm-ostree");
    cmd.arg("status").env("RPMOSTREE_CLIENT_ID", "zincati");

    // Try to request the minimum scope we need.
    if booted_only {
        cmd.arg("--booted");
    }

    let cmdrun = cmd
        .arg("--json")
        .output()
        .with_context(|_| "failed to run 'rpm-ostree' binary")?;

    if !cmdrun.status.success() {
        RPM_OSTREE_STATUS_FAILURES.inc();
        bail!(
            "rpm-ostree status failed:\n{}",
            String::from_utf8_lossy(&cmdrun.stderr)
        );
    }
    let status: StatusJSON = serde_json::from_slice(&cmdrun.stdout)?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_status(path: &str) -> Fallible<StatusJSON> {
        let fp = std::fs::File::open(path).unwrap();
        let bufrd = std::io::BufReader::new(fp);
        let status: StatusJSON = serde_json::from_reader(bufrd)?;
        Ok(status)
    }

    #[test]
    fn mock_deployments() {
        {
            let status = mock_status("tests/fixtures/rpm-ostree-status.json").unwrap();
            let deployments = parse_local_deployments(&status, false).unwrap();
            assert_eq!(deployments.len(), 1);
        }
        {
            let status = mock_status("tests/fixtures/rpm-ostree-staged.json").unwrap();
            let deployments = parse_local_deployments(&status, false).unwrap();
            assert_eq!(deployments.len(), 2);
        }
        {
            let status = mock_status("tests/fixtures/rpm-ostree-staged.json").unwrap();
            let deployments = parse_local_deployments(&status, true).unwrap();
            assert_eq!(deployments.len(), 1);
        }
    }

    #[test]
    fn mock_booted_basearch() {
        let status = mock_status("tests/fixtures/rpm-ostree-status.json").unwrap();
        let booted = booted_json(&status).unwrap();
        assert_eq!(booted.base_metadata.basearch, "x86_64");
    }

    #[test]
    fn mock_booted_updates_stream() {
        let status = mock_status("tests/fixtures/rpm-ostree-status.json").unwrap();
        let booted = booted_json(&status).unwrap();
        assert_eq!(booted.base_metadata.stream, "testing-devel");
    }
}
