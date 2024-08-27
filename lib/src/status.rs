use std::collections::VecDeque;
use std::io::IsTerminal;
use std::io::Write;

use anyhow::{Context, Result, Error};
use camino::Utf8Path;
use fn_error_context::context;
use ostree::glib;
use ostree_container::OstreeImageReference;
use ostree_ext::container as ostree_container;
use ostree_ext::keyfileext::KeyFileExt;
use ostree_ext::oci_spec;
use ostree_ext::ostree;

use crate::cli::OutputFormat;
use crate::spec::{BootEntry, BootOrder, Host, HostSpec, HostStatus, HostType};
use crate::spec::{ImageReference, ImageSignature};
use crate::store::{CachedImageStatus, ContainerImageStore, Storage};

impl From<ostree_container::SignatureSource> for ImageSignature {
    fn from(sig: ostree_container::SignatureSource) -> Self {
        use ostree_container::SignatureSource;
        match sig {
            SignatureSource::OstreeRemote(r) => Self::OstreeRemote(r),
            SignatureSource::ContainerPolicy => Self::ContainerPolicy,
            SignatureSource::ContainerPolicyAllowInsecure => Self::Insecure,
        }
    }
}

impl From<ImageSignature> for ostree_container::SignatureSource {
    fn from(sig: ImageSignature) -> Self {
        use ostree_container::SignatureSource;
        match sig {
            ImageSignature::OstreeRemote(r) => SignatureSource::OstreeRemote(r),
            ImageSignature::ContainerPolicy => Self::ContainerPolicy,
            ImageSignature::Insecure => Self::ContainerPolicyAllowInsecure,
        }
    }
}

/// Fixme lower serializability into ostree-ext
fn transport_to_string(transport: ostree_container::Transport) -> String {
    match transport {
        // Canonicalize to registry for our own use
        ostree_container::Transport::Registry => "registry".to_string(),
        o => {
            let mut s = o.to_string();
            s.truncate(s.rfind(':').unwrap());
            s
        }
    }
}

impl From<OstreeImageReference> for ImageReference {
    fn from(imgref: OstreeImageReference) -> Self {
        let signature = match imgref.sigverify {
            ostree_container::SignatureSource::ContainerPolicyAllowInsecure => None,
            v => Some(v.into()),
        };
        Self {
            signature,
            transport: transport_to_string(imgref.imgref.transport),
            image: imgref.imgref.name,
        }
    }
}

impl From<ImageReference> for OstreeImageReference {
    fn from(img: ImageReference) -> Self {
        let sigverify = match img.signature {
            Some(v) => v.into(),
            None => ostree_container::SignatureSource::ContainerPolicyAllowInsecure,
        };
        Self {
            sigverify,
            imgref: ostree_container::ImageReference {
                // SAFETY: We validated the schema in kube-rs
                transport: img.transport.as_str().try_into().unwrap(),
                name: img.image,
            },
        }
    }
}

/// Parse an ostree origin file (a keyfile) and extract the targeted
/// container image reference.
fn get_image_origin(origin: &glib::KeyFile) -> Result<Option<OstreeImageReference>> {
    origin
        .optional_string("origin", ostree_container::deploy::ORIGIN_CONTAINER)
        .context("Failed to load container image from origin")?
        .map(|v| ostree_container::OstreeImageReference::try_from(v.as_str()))
        .transpose()
}

pub(crate) struct Deployments {
    pub(crate) staged: Option<ostree::Deployment>,
    pub(crate) rollback: Option<ostree::Deployment>,
    #[allow(dead_code)]
    pub(crate) other: VecDeque<ostree::Deployment>,
}

pub(crate) fn try_deserialize_timestamp(t: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    match chrono::DateTime::parse_from_rfc3339(t).context("Parsing timestamp") {
        Ok(t) => Some(t.into()),
        Err(e) => {
            tracing::warn!("Invalid timestamp in image: {:#}", e);
            None
        }
    }
}

pub(crate) fn labels_of_config(
    config: &oci_spec::image::ImageConfiguration,
) -> Option<&std::collections::HashMap<String, String>> {
    config.config().as_ref().and_then(|c| c.labels().as_ref())
}

/// Given an OSTree deployment, parse out metadata into our spec.
#[context("Reading deployment metadata")]
fn boot_entry_from_deployment(
    sysroot: &Storage,
    deployment: &ostree::Deployment,
) -> Result<BootEntry> {
    let (
        store,
        CachedImageStatus {
            image,
            cached_update,
        },
        incompatible,
    ) = if let Some(origin) = deployment.origin().as_ref() {
        let incompatible = crate::utils::origin_has_rpmostree_stuff(origin);
        let (store, cached_imagestatus) = if incompatible {
            // If there are local changes, we can't represent it as a bootc compatible image.
            (None, CachedImageStatus::default())
        } else if let Some(image) = get_image_origin(origin)? {
            let store = deployment.store()?;
            let store = store.as_ref().unwrap_or(&sysroot.store);
            let spec = Some(store.spec());
            let status = store.imagestatus(sysroot, deployment, image)?;

            (spec, status)
        } else {
            // The deployment isn't using a container image
            (None, CachedImageStatus::default())
        };
        (store, cached_imagestatus, incompatible)
    } else {
        // The deployment has no origin at all (this generally shouldn't happen)
        (None, CachedImageStatus::default(), false)
    };

    let r = BootEntry {
        image,
        cached_update,
        incompatible,
        store,
        pinned: deployment.is_pinned(),
        ostree: Some(crate::spec::BootEntryOstree {
            checksum: deployment.csum().into(),
            // SAFETY: The deployserial is really unsigned
            deploy_serial: deployment.deployserial().try_into().unwrap(),
        }),
    };
    Ok(r)
}

impl BootEntry {
    /// Given a boot entry, find its underlying ostree container image
    pub(crate) fn query_image(
        &self,
        repo: &ostree::Repo,
    ) -> Result<Option<Box<ostree_container::store::LayeredImageState>>> {
        if self.image.is_none() {
            return Ok(None);
        }
        if let Some(checksum) = self.ostree.as_ref().map(|c| c.checksum.as_str()) {
            ostree_container::store::query_image_commit(repo, checksum).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// A variant of [`get_status`] that requires a booted deployment.
pub(crate) fn get_status_require_booted(
    sysroot: &Storage,
) -> Result<(ostree::Deployment, Deployments, Host)> {
    let booted_deployment = sysroot.require_booted_deployment()?;
    let (deployments, host) = get_status(sysroot, Some(&booted_deployment))?;
    Ok((booted_deployment, deployments, host))
}

/// Gather the ostree deployment objects, but also extract metadata from them into
/// a more native Rust structure.
#[context("Computing status")]
pub(crate) fn get_status(
    sysroot: &Storage,
    booted_deployment: Option<&ostree::Deployment>,
) -> Result<(Deployments, Host)> {
    let stateroot = booted_deployment.as_ref().map(|d| d.osname());
    let (mut related_deployments, other_deployments) = sysroot
        .deployments()
        .into_iter()
        .partition::<VecDeque<_>, _>(|d| Some(d.osname()) == stateroot);
    let staged = related_deployments
        .iter()
        .position(|d| d.is_staged())
        .map(|i| related_deployments.remove(i).unwrap());
    tracing::debug!("Staged: {staged:?}");
    // Filter out the booted, the caller already found that
    if let Some(booted) = booted_deployment.as_ref() {
        related_deployments.retain(|f| !f.equal(booted));
    }
    let rollback = related_deployments.pop_front();
    let rollback_queued = match (booted_deployment.as_ref(), rollback.as_ref()) {
        (Some(booted), Some(rollback)) => rollback.index() < booted.index(),
        _ => false,
    };
    let boot_order = if rollback_queued {
        BootOrder::Rollback
    } else {
        BootOrder::Default
    };
    tracing::debug!("Rollback queued={rollback_queued:?}");
    let other = {
        related_deployments.extend(other_deployments);
        related_deployments
    };
    let deployments = Deployments {
        staged,
        rollback,
        other,
    };

    let staged = deployments
        .staged
        .as_ref()
        .map(|d| boot_entry_from_deployment(sysroot, d))
        .transpose()
        .context("Staged deployment")?;
    let booted = booted_deployment
        .as_ref()
        .map(|d| boot_entry_from_deployment(sysroot, d))
        .transpose()
        .context("Booted deployment")?;
    let rollback = deployments
        .rollback
        .as_ref()
        .map(|d| boot_entry_from_deployment(sysroot, d))
        .transpose()
        .context("Rollback deployment")?;
    let spec = staged
        .as_ref()
        .or(booted.as_ref())
        .and_then(|entry| entry.image.as_ref())
        .map(|img| HostSpec {
            image: Some(img.image.clone()),
            boot_order,
        })
        .unwrap_or_default();

    let ty = if booted
        .as_ref()
        .map(|b| b.image.is_some())
        .unwrap_or_default()
    {
        // We're only of type BootcHost if we booted via container image
        Some(HostType::BootcHost)
    } else {
        None
    };

    let mut host = Host::new(spec);
    host.status = HostStatus {
        staged,
        booted,
        rollback,
        rollback_queued,
        ty,
    };
    Ok((deployments, host))
}

/// Implementation of the `bootc status` CLI command.
#[context("Status")]
pub(crate) async fn status(opts: super::cli::StatusOpts) -> Result<()> {
    match opts.format_version.unwrap_or_default() {
        0 => {}
        o => anyhow::bail!("Unsupported format version: {o}"),
    };
    let host = if !Utf8Path::new("/run/ostree-booted").try_exists()? {
        Default::default()
    } else {
        let sysroot = super::cli::get_storage().await?;
        let booted_deployment = sysroot.booted_deployment();
        let (_deployments, host) = get_status(&sysroot, booted_deployment.as_ref())?;
        host
    };

    // If we're in JSON mode, then convert the ostree data into Rust-native
    // structures that can be serialized.
    // Filter to just the serializable status structures.
    let out = std::io::stdout();
    let mut out = out.lock();
    let legacy_opt = if opts.json {
        OutputFormat::Json
    } else {
        if  std::io::stdout().is_terminal() {
            OutputFormat::HumanReadable
        } else {
            OutputFormat::Yaml
        }
    };
    let format = opts.format.unwrap_or(legacy_opt);
    match format {
        OutputFormat::Json => serde_json::to_writer(&mut out, &host).map_err(anyhow::Error::new),
        OutputFormat::Yaml => serde_yaml::to_writer(&mut out, &host).map_err(anyhow::Error::new),
        OutputFormat::HumanReadable => human_readable_output_beta(&mut out, &host),  
    }
    .context("Writing to stdout")?;

    Ok(())
}

fn human_readable_output(mut out: impl Write, host: &Host) -> Result<()> {
    for (print_value, status) in [
        ("staged", &host.status.staged),
        ("booted", &host.status.booted),
        ("rollback", &host.status.rollback),
    ] {
        if let Some(host_status) = status {
            if let Some(image) = &host_status.image {
                let image_print = format!("Current {print_value} image: {:?}", image.image.image);
                out.write_all(image_print.as_bytes())?;
            } else {
                out.write_all(format!("No image defined").as_bytes())?;
            }
        }
        else {
            out.write_all(format!("No {print_value} image present").as_bytes())?;
        }
    }
    Ok(())
}

fn human_readable_output_beta(mut out: impl Write, host: &Host) -> Result<()> {
    for (print_value, status) in [
        ("staged", &host.status.staged),
        ("booted", &host.status.booted),
        ("rollback", &host.status.rollback),
    ] {
        if let Some(host_status) = status {
            if let Some(image) = &host_status.image {
                if let Some(version) = &image.version {
                    if let Some(signature) = &image.image.signature {
                        let image_print = format!(
                            "Current {:?} image: {:?} \n
                            Image version: {:?} \n
                            Image transport: {:?} \n
                            Image signature: {:?} \n
                            Image digest: {:?} \n
                            ", 
                            print_value, 
                            image.image.image, 
                            version,
                            image.image.transport,
                            signature,
                            image.image_digest,
                        );
                        out.write_all(image_print.as_bytes())?;
                    } else {
                        out.write_all(format!("No image signature defined \n").as_bytes())?;
                    }
                } else {
                    out.write_all(format!("No image version defined \n").as_bytes())?;
                }
            } else {
                out.write_all(format!("No image defined \n").as_bytes())?;
            }
        }
        else {
            out.write_all(format!("No {print_value} image present \n").as_bytes())?;
        }
    }
    Ok(())
}

#[test]
fn test_human_readable() {
    // Tests Staged and Booted, null Rollback
    let mut SPEC_FIXTURE: &str = include_str!("fixtures/spec.yaml");
    let mut host: Host = serde_yaml::from_str(SPEC_FIXTURE).unwrap();
    let mut w = Vec::new();
    human_readable_output_beta(&mut w, &host).unwrap();
    let w = String::from_utf8(w).unwrap();
    dbg!(&w);
    assert!(w.contains("quay.io/example/someimage:latest"));

    // Basic rhel for edge bootc install with nothing
    SPEC_FIXTURE = include_str!("fixtures/spec-rfe-ostree-deployment.yaml");
    host = serde_yaml::from_str(SPEC_FIXTURE).unwrap();
    let mut w = Vec::new();
    human_readable_output_beta(&mut w, &host).unwrap();
    let w = String::from_utf8(w).unwrap();
    dbg!(&w);
    // Spec contains no image, need to update once human_readable_output is more robust
    assert!(w.contains(""));

    // staged image, no boot/rollback
    SPEC_FIXTURE = include_str!("fixtures/spec-ostree-to-bootc.yaml");
    host = serde_yaml::from_str(SPEC_FIXTURE).unwrap();
    let mut w = Vec::new();
    human_readable_output_beta(&mut w, &host).unwrap();
    let w = String::from_utf8(w).unwrap();
    dbg!(&w);
    assert!(w.contains("quay.io/centos-bootc/centos-bootc:stream9"));

    // booted image, no staged/rollback
    SPEC_FIXTURE = include_str!("fixtures/spec-ostree-to-bootc.yaml");
    host = serde_yaml::from_str(SPEC_FIXTURE).unwrap();
    let mut w = Vec::new();
    human_readable_output_beta(&mut w, &host).unwrap();
    let w = String::from_utf8(w).unwrap();
    dbg!(&w);
    assert!(w.contains("quay.io/centos-bootc/centos-bootc:stream9"));
}

#[test]
fn test_convert_signatures() {
    use std::str::FromStr;
    let ir_unverified = &OstreeImageReference::from_str(
        "ostree-unverified-registry:quay.io/someexample/foo:latest",
    )
    .unwrap();
    let ir_ostree = &OstreeImageReference::from_str(
        "ostree-remote-registry:fedora:quay.io/fedora/fedora-coreos:stable",
    )
    .unwrap();

    let ir = ImageReference::from(ir_unverified.clone());
    assert_eq!(ir.image, "quay.io/someexample/foo:latest");
    assert_eq!(ir.signature, None);

    let ir = ImageReference::from(ir_ostree.clone());
    assert_eq!(ir.image, "quay.io/fedora/fedora-coreos:stable");
    assert_eq!(
        ir.signature,
        Some(ImageSignature::OstreeRemote("fedora".into()))
    );
}
