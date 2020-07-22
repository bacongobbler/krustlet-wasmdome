//! A custom krustlet backend that can run [waSCC](https://wascc.dev/) based workloads.
//!
//! The crate provides the [`Provider`] type which can be used as a provider with [`kubelet`].

#![deny(missing_docs)]

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{ContainerStatus as KubeContainerStatus, Pod as KubePod};
use kube::{api::DeleteParams, Api};
use kubelet::container::{Handle as ContainerHandle, Status as ContainerStatus};
use kubelet::handle::StopHandler;
use kubelet::node::Builder;
use kubelet::pod::{key_from_pod, pod_key, Handle};
use kubelet::pod::{update_status, Phase, Pod, Status as PodStatus};
use kubelet::provider::Provider as KubeProvider;
use kubelet::provider::ProviderError;
use kubelet::store::Store;
use kubelet::volume::Ref;
use log::{debug, error, info, trace};
use tempfile::NamedTempFile;
use tokio::sync::watch::{self, Receiver};
use tokio::sync::RwLock;
use wascc_fs::FileSystemProvider;
use wascc_host::{Actor, NativeCapability, WasccHost};
use engine_provider::WasmdomeEngineProvider;

use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The architecture that the pod targets.
const TARGET_WASM32_WASCC: &str = "wasm32-wascc";

/// The name of the Filesystem capability.
const FS_CAPABILITY: &str = "wascc:blobstore";

/// The root directory of waSCC logs.
const LOG_DIR_NAME: &str = "wascc-logs";

/// The key used to define the root directory of the Filesystem capability.
const FS_CONFIG_ROOTDIR: &str = "ROOT";

/// The root directory of waSCC volumes.
const VOLUME_DIR: &str = "volumes";

/// Kubernetes' view of environment variables is an unordered map of string to string.
type EnvVars = std::collections::HashMap<String, String>;

/// A [kubelet::handle::Handle] implementation for a wascc actor
pub struct ActorHandle {
    /// The public key of the wascc Actor that will be stopped
    pub key: String,
    host: Arc<Mutex<WasccHost>>,
    volumes: Vec<VolumeBinding>,
}

#[async_trait::async_trait]
impl StopHandler for ActorHandle {
    async fn stop(&mut self) -> anyhow::Result<()> {
        debug!("stopping wascc instance {}", self.key);
        let host = self.host.clone();
        let key = self.key.clone();
        let volumes: Vec<VolumeBinding> = self.volumes.drain(0..).collect();
        tokio::task::spawn_blocking(move || {
            let lock = host.lock().unwrap();
            lock.remove_actor(&key)
                .map_err(|e| anyhow::anyhow!("unable to remove actor: {:?}", e))?;
            for volume in volumes.into_iter() {
                lock.remove_native_capability(FS_CAPABILITY, Some(volume.name))
                    .map_err(|e| anyhow::anyhow!("unable to remove volume capability: {:?}", e))?;
            }
            Ok(())
        })
        .await?
    }

    async fn wait(&mut self) -> anyhow::Result<()> {
        // TODO: Figure out if there is a way to wait for an actor to be removed
        Ok(())
    }
}

/// Provider provides a Kubelet runtime implementation that executes WASM binaries.
///
/// Currently, this runtime uses WASCC as a host, loading the primary container as an actor.
/// TODO: In the future, we will look at loading capabilities using the "sidecar" metaphor
/// from Kubernetes.
#[derive(Clone)]
pub struct Provider<S> {
    handles: Arc<RwLock<HashMap<String, Handle<ActorHandle, LogHandleFactory>>>>,
    store: S,
    volume_path: PathBuf,
    log_path: PathBuf,
    kubeconfig: kube::Config,
    host: Arc<Mutex<WasccHost>>,
}

impl<S: Store + Send + Sync> Provider<S> {
    /// Returns a new wasCC provider configured to use the proper data directory
    /// (including creating it if necessary)
    pub async fn new(
        store: S,
        config: &kubelet::config::Config,
        kubeconfig: kube::Config,
    ) -> anyhow::Result<Self> {
        let host = Arc::new(Mutex::new(WasccHost::new()));
        let log_path = config.data_dir.join(LOG_DIR_NAME);
        let volume_path = config.data_dir.join(VOLUME_DIR);
        tokio::fs::create_dir_all(&log_path).await?;
        tokio::fs::create_dir_all(&volume_path).await?;

        // wascc has native and portable capabilities.
        //
        // Native capabilities are either dynamic libraries (.so, .dylib, .dll)
        // or statically linked Rust libaries. If the native capabilty is a dynamic
        // library it must be loaded and configured through [`NativeCapability::from_file`].
        // If it is a statically linked libary it can be configured through
        // [`NativeCapability::from_instance`].
        //
        // Portable capabilities are WASM modules.  Portable capabilities
        // don't fully work, and won't until the WASI spec has matured.
        //
        // Here we are using the native capabilties as statically linked libraries that will
        // be compiled into the wascc-provider binary.
        let cloned_host = host.clone();
        tokio::task::spawn_blocking(move || {
            info!("Loading Wasmdome capability");
            let wasmdome_provider = WasmdomeEngineProvider::new();
            let data = NativeCapability::from_instance(wasmdome_provider, None)
                .map_err(|e| anyhow::anyhow!("Failed to instantiate wasmdome capability: {}", e))?;
            cloned_host
                .lock()
                .unwrap()
                .add_native_capability(data)
                .map_err(|e| anyhow::anyhow!("Failed to add wasmdome capability: {}", e))
        })
        .await??;
        Ok(Self {
            handles: Default::default(),
            store,
            volume_path,
            log_path,
            kubeconfig,
            host,
        })
    }
}

#[async_trait]
impl<S: Store + Send + Sync> KubeProvider for Provider<S> {
    const ARCH: &'static str = TARGET_WASM32_WASCC;

    async fn node(&self, builder: &mut Builder) -> anyhow::Result<()> {
        builder.set_architecture("wasm-wasi");
        builder.add_taint("NoExecute", "krustlet/arch", Self::ARCH);
        Ok(())
    }

    async fn add(&self, pod: Pod) -> anyhow::Result<()> {
        // To run an Add event, we load the actor, and update the pod status
        // to Running.  The wascc runtime takes care of starting the actor.
        // When the pod finishes, we update the status to Succeeded unless it
        // produces an error, in which case we mark it Failed.
        debug!("Pod added {:?}", pod.name());

        info!("Starting containers for pod {:?}", pod.name());
        let mut modules = self.store.fetch_pod_modules(&pod).await?;
        let mut container_handles = HashMap::new();
        let client = kube::Client::new(self.kubeconfig.clone());
        let volumes = Ref::volumes_from_pod(&self.volume_path, &pod, &client).await?;
        for container in pod.containers() {
            let volume_bindings: Vec<VolumeBinding> =
                if let Some(volume_mounts) = container.volume_mounts.as_ref() {
                    volume_mounts
                        .iter()
                        .map(|vm| -> anyhow::Result<VolumeBinding> {
                            // Check the volume exists first
                            let vol = volumes.get(&vm.name).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "no volume with the name of {} found for container {}",
                                    vm.name,
                                    container.name
                                )
                            })?;
                            // We can safely assume that this should be valid UTF-8 because it would have
                            // been validated by the k8s API
                            Ok(VolumeBinding {
                                name: vm.name.clone(),
                                host_path: vol.deref().clone(),
                            })
                        })
                        .collect::<anyhow::Result<_>>()?
                } else {
                    vec![]
                };

            debug!("Starting container {} on thread", container.name);

            let module_data = modules
                .remove(&container.name)
                .expect("FATAL ERROR: module map not properly populated");
            let lp = self.log_path.clone();
            let (status_sender, status_recv) = watch::channel(ContainerStatus::Waiting {
                timestamp: chrono::Utc::now(),
                message: "No status has been received from the process".into(),
            });
            let host = self.host.clone();
            let wascc_result = tokio::task::spawn_blocking(move || {
                wascc_run(host, module_data, &mut Vec::new(), volume_bindings, &lp, status_recv)
            })
            .await?;
            match wascc_result {
                Ok(handle) => {
                    container_handles.insert(container.name.clone(), handle);
                    status_sender
                        .broadcast(ContainerStatus::Running {
                            timestamp: chrono::Utc::now(),
                        })
                        .expect("status should be able to send");
                }
                Err(e) => {
                    // We can't broadcast here because the receiver has been dropped at this point
                    // (it was never used in creating a runtime handle)
                    let mut container_statuses = HashMap::new();
                    container_statuses.insert(
                        container.name.clone(),
                        ContainerStatus::Terminated {
                            timestamp: chrono::Utc::now(),
                            failed: true,
                            message: format!("Error while starting container: {:?}", e),
                        },
                    );
                    let status = PodStatus {
                        message: None,
                        container_statuses,
                    };
                    pod.patch_status(client.clone(), status).await;
                    return Err(anyhow::anyhow!("Failed to run pod: {}", e));
                }
            }
        }
        info!(
            "All containers started for pod {:?}. Updating status",
            pod.name()
        );
        // Wrap this in a block so the write lock goes out of scope when we are done
        {
            let mut handles = self.handles.write().await;
            handles.insert(
                key_from_pod(&pod),
                Handle::new(container_handles, pod, client, None)?,
            );
        }

        Ok(())
    }

    async fn modify(&self, pod: Pod) -> anyhow::Result<()> {
        // The only things we care about are:
        // 1. metadata.deletionTimestamp => signal all containers to stop and then mark them
        //    as terminated
        // 2. spec.containers[*].image, spec.initContainers[*].image => stop the currently
        //    running containers and start new ones?
        // 3. spec.activeDeadlineSeconds => Leaving unimplemented for now
        // TODO: Determine what the proper behavior should be if labels change
        let pod_name = pod.name().to_owned();
        let pod_namespace = pod.namespace().to_owned();
        debug!(
            "Got pod modified event for {} in namespace {}",
            pod_name, pod_namespace
        );
        trace!("Modified pod spec: {:#?}", pod.as_kube_pod());
        if let Some(_timestamp) = pod.deletion_timestamp() {
            debug!(
                "Found delete timestamp for pod {} in namespace {}. Stopping running actors",
                pod_name, pod_namespace
            );
            let mut handles = self.handles.write().await;
            match handles.get_mut(&key_from_pod(&pod)) {
                Some(h) => {
                    h.stop().await?;

                    debug!(
                        "All actors stopped for pod {} in namespace {}, updating status",
                        pod_name, pod_namespace
                    );
                    // Having to do this here isn't my favorite thing, but we need to update the
                    // status of the container so it can be deleted. We will probably need to have
                    // some sort of provider that can send a message about status to the Kube API
                    let now = chrono::Utc::now();
                    let terminated = ContainerStatus::Terminated {
                        timestamp: now,
                        message: "Pod stopped".to_owned(),
                        failed: false,
                    };

                    let container_statuses: Vec<KubeContainerStatus> = pod
                        .into_kube_pod()
                        .spec
                        .unwrap_or_default()
                        .containers
                        .into_iter()
                        .map(|c| terminated.to_kubernetes(c.name))
                        .collect();

                    let json_status = serde_json::json!(
                        {
                            "metadata": {
                                "resourceVersion": "",
                            },
                            "status": {
                                "message": "Pod stopped",
                                "phase": Phase::Succeeded,
                                "containerStatuses": container_statuses,
                            }
                        }
                    );
                    let client = kube::client::Client::new(self.kubeconfig.clone());
                    update_status(client.clone(), &pod_namespace, &pod_name, &json_status).await?;

                    let pod_client: Api<KubePod> = Api::namespaced(client.clone(), &pod_namespace);
                    let dp = DeleteParams {
                        grace_period_seconds: Some(0),
                        ..Default::default()
                    };
                    match pod_client.delete(&pod_name, &dp).await {
                        Ok(_) => Ok(()),
                        Err(e) => Err(e.into()),
                    }
                }
                None => {
                    // This isn't an error with the pod, so don't return an error (otherwise it will
                    // get updated in its status). This is an unlikely case to get into and means
                    // that something is likely out of sync, so just log the error
                    error!(
                        "Unable to find pod {} in namespace {} when trying to stop all containers",
                        pod_name, pod_namespace
                    );
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
        // TODO: Implement behavior for stopping old containers and restarting when the container
        // image changes
    }

    async fn delete(&self, pod: Pod) -> anyhow::Result<()> {
        let mut handles = self.handles.write().await;
        match handles.remove(&key_from_pod(&pod)) {
            Some(_) => debug!(
                "Pod {} in namespace {} removed",
                pod.name(),
                pod.namespace()
            ),
            None => info!(
                "unable to find pod {} in namespace {}, it was likely already deleted",
                pod.name(),
                pod.namespace()
            ),
        }
        Ok(())
    }

    async fn logs(
        &self,
        namespace: String,
        pod_name: String,
        container_name: String,
        sender: kubelet::log::Sender,
    ) -> anyhow::Result<()> {
        let mut handles = self.handles.write().await;
        let handle = handles
            .get_mut(&pod_key(&namespace, &pod_name))
            .ok_or_else(|| ProviderError::PodNotFound {
                pod_name: pod_name.clone(),
            })?;
        handle.output(&container_name, sender).await
    }
}

struct VolumeBinding {
    name: String,
    host_path: PathBuf,
}

/// Capability describes a waSCC capability.
///
/// Capabilities are made available to actors through a two-part processthread:
/// - They must be registered
/// - For each actor, the capability must be configured
struct Capability {
    name: &'static str,
    binding: Option<String>,
    env: EnvVars,
}

/// Holds our tempfile handle.
struct LogHandleFactory {
    temp: NamedTempFile,
}

impl kubelet::log::HandleFactory<tokio::fs::File> for LogHandleFactory {
    /// Creates `tokio::fs::File` on demand for log reading.
    fn new_handle(&self) -> tokio::fs::File {
        tokio::fs::File::from_std(self.temp.reopen().unwrap())
    }
}

/// Run the given WASM data as a waSCC actor with the given public key.
///
/// The provided capabilities will be configured for this actor, but the capabilities
/// must first be loaded into the host by some other process, such as register_native_capabilities().
fn wascc_run(
    host: Arc<Mutex<WasccHost>>,
    data: Vec<u8>,
    capabilities: &mut Vec<Capability>,
    volumes: Vec<VolumeBinding>,
    log_path: &Path,
    status_recv: Receiver<ContainerStatus>,
) -> anyhow::Result<ContainerHandle<ActorHandle, LogHandleFactory>> {
    info!("sending actor to wascc host");
    let log_output = NamedTempFile::new_in(log_path)?;
    let mut logenv: HashMap<String, String> = HashMap::new();
    logenv.insert(
        "LOG_PATH".to_string(),
        log_output.path().to_str().unwrap().to_owned(),
    );

    let load = Actor::from_bytes(data).map_err(|e| anyhow::anyhow!("Error loading WASM: {}", e))?;
    let pk = load.public_key();

    if load.capabilities().contains(&FS_CAPABILITY.to_owned()) {
        for vol in &volumes {
            info!(
                "Loading File System capability for volume name: '{}' host_path: '{}'",
                vol.name,
                vol.host_path.display()
            );
            let mut fsenv: HashMap<String, String> = HashMap::new();
            fsenv.insert(
                FS_CONFIG_ROOTDIR.to_owned(),
                vol.host_path.as_path().to_str().unwrap().to_owned(),
            );
            let fs_provider = FileSystemProvider::new();
            let fs_capability =
                NativeCapability::from_instance(fs_provider, Some(vol.name.clone())).map_err(
                    |e| anyhow::anyhow!("Failed to instantiate File System capability: {}", e),
                )?;
            host.lock()
                .unwrap()
                .add_native_capability(fs_capability)
                .map_err(|e| anyhow::anyhow!("Failed to add File System capability: {}", e))?;
            capabilities.push(Capability {
                name: FS_CAPABILITY,
                binding: Some(vol.name.clone()),
                env: fsenv,
            });
        }
    }

    host.lock()
        .unwrap()
        .add_actor(load)
        .map_err(|e| anyhow::anyhow!("Error adding actor: {}", e))?;
    capabilities.iter().try_for_each(|cap| {
        info!("configuring capability {}", cap.name);
        host.lock()
            .unwrap()
            .bind_actor(&pk, cap.name, cap.binding.clone(), cap.env.clone())
            .map_err(|e| anyhow::anyhow!("Error configuring capabilities for module: {}", e))
    })?;

    let log_handle_factory = LogHandleFactory { temp: log_output };

    info!("wascc actor executing");
    Ok(ContainerHandle::new(
        ActorHandle {
            host,
            key: pk,
            volumes,
        },
        log_handle_factory,
        status_recv,
    ))
}
