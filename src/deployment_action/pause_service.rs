use crate::cloud_provider::DeploymentTarget;
use crate::deployment_action::{DeploymentAction, K8sResourceType};
use crate::errors::{CommandError, EngineError};
use crate::events::EventDetails;
use crate::runtime::block_on;
use jsonptr::Pointer;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::autoscaling::v1::{Scale, ScaleSpec};
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::wait::{await_condition, Condition};
use kube::{Api, Client};
use serde_json::Value;
use std::time::Duration;

fn has_deployment_ready_replicas(nb_ready_replicas: usize) -> impl Condition<Deployment> {
    move |deployment: Option<&Deployment>| {
        deployment
            .and_then(|d| d.status.as_ref())
            .and_then(|status| status.ready_replicas.as_ref())
            .unwrap_or(&0)
            == &(nb_ready_replicas as i32)
    }
}

fn has_statefulset_ready_replicas(nb_ready_replicas: usize) -> impl Condition<StatefulSet> {
    move |deployment: Option<&StatefulSet>| {
        deployment
            .and_then(|d| d.status.as_ref())
            .and_then(|status| status.ready_replicas.as_ref())
            .unwrap_or(&0)
            == &(nb_ready_replicas as i32)
    }
}

fn has_cron_job_suspended_value(suspend: bool) -> impl Condition<CronJob> {
    move |cron_job: Option<&CronJob>| {
        cron_job
            .and_then(|d| d.spec.as_ref())
            .and_then(|spec| spec.suspend.as_ref())
            == Some(&suspend)
    }
}

async fn pause_service(
    kube: &kube::Client,
    namespace: &str,
    selector: &str,
    desired_size: usize, // only for test, normal behavior assume 0
    k8s_resource_type: K8sResourceType,
    is_cluster_wide_resources_allowed: bool,
) -> Result<(), kube::Error> {
    // We don't need to remove HPA, if we set desired replicas to 0, hpa disable itself until we change it back
    // https://kubernetes.io/docs/tasks/run-application/horizontal-pod-autoscale/#implicit-maintenance-mode-deactivation

    match k8s_resource_type {
        K8sResourceType::StateFulSet => {
            let (list_params, patch_params, patch) = get_patch_merge(selector, desired_size);
            let statefulsets: Api<StatefulSet> = if is_cluster_wide_resources_allowed {
                Api::all(kube.clone())
            } else {
                Api::namespaced(kube.clone(), namespace)
            };
            for statefulset in statefulsets.list(&list_params).await? {
                if let (Some(namespace), Some(name)) = (statefulset.metadata.namespace, statefulset.metadata.name) {
                    let statefulsets: Api<StatefulSet> = Api::namespaced(kube.clone(), &namespace); // patch_scale need to have statefulsets with namespace
                    statefulsets.patch_scale(&name, &patch_params, &patch).await?;
                    let _ = await_condition(statefulsets.clone(), &name, has_statefulset_ready_replicas(0)).await;
                }
            }
            wait_for_pods_to_be_in_correct_state(
                kube,
                namespace,
                desired_size,
                is_cluster_wide_resources_allowed,
                &list_params,
            )
            .await;
        }
        K8sResourceType::Deployment => {
            let (list_params, patch_params, patch) = get_patch_merge(selector, desired_size);
            let deployments: Api<Deployment> = if is_cluster_wide_resources_allowed {
                Api::all(kube.clone())
            } else {
                Api::namespaced(kube.clone(), namespace)
            };
            for deployment in deployments.list(&list_params).await? {
                if let (Some(namespace), Some(name)) = (deployment.metadata.namespace, deployment.metadata.name) {
                    let deployments: Api<Deployment> = Api::namespaced(kube.clone(), &namespace); // patch_scale needs to have deployments with namespace
                    deployments.patch_scale(&name, &patch_params, &patch).await?;
                    let _ = await_condition(deployments.clone(), &name, has_deployment_ready_replicas(0)).await;
                }
            }
            wait_for_pods_to_be_in_correct_state(
                kube,
                namespace,
                desired_size,
                is_cluster_wide_resources_allowed,
                &list_params,
            )
            .await;
        }
        K8sResourceType::CronJob => {
            let (list_params, patch_params, patch) = get_patch_suspend(selector, desired_size == 0);
            let cron_jobs: Api<CronJob> = if is_cluster_wide_resources_allowed {
                Api::all(kube.clone())
            } else {
                Api::namespaced(kube.clone(), namespace)
            };
            for cron_job in cron_jobs.list(&list_params).await? {
                if let (Some(namespace), Some(name)) = (cron_job.metadata.namespace, cron_job.metadata.name) {
                    let cron_jobs: Api<CronJob> = Api::namespaced(kube.clone(), &namespace); // patch needs to have cron_jobs with namespace
                    cron_jobs.patch(&name, &patch_params, &patch).await?;
                    let _ = await_condition(cron_jobs.clone(), &name, has_cron_job_suspended_value(desired_size == 0))
                        .await;
                }
            }
        }
        K8sResourceType::DaemonSet => {}
        K8sResourceType::Job => {}
    };

    Ok(())
}

async fn unpause_service_if_needed(
    kube: &kube::Client,
    namespace: &str,
    selector: &str,
    k8s_resource_type: K8sResourceType,
    is_cluster_wide_resources_allowed: bool,
) -> Result<(), kube::Error> {
    match k8s_resource_type {
        K8sResourceType::StateFulSet => {
            let (list_params, patch_params, patch) = get_patch_merge(selector, 1);
            let statefulsets: Api<StatefulSet> = if is_cluster_wide_resources_allowed {
                Api::all(kube.clone())
            } else {
                Api::namespaced(kube.clone(), namespace)
            };
            for statefulset in statefulsets.list(&list_params).await? {
                if statefulset.status.map(|s| s.replicas).unwrap_or(0) == 0 {
                    if let (Some(namespace), Some(name)) = (statefulset.metadata.namespace, statefulset.metadata.name) {
                        let statefulsets: Api<StatefulSet> = Api::namespaced(kube.clone(), &namespace); // patch_scale needs to have statefulsets with namespace
                        statefulsets.patch_scale(&name, &patch_params, &patch).await?;
                    }
                }
            }
        }
        K8sResourceType::Deployment => {
            let (list_params, patch_params, patch) = get_patch_merge(selector, 1);
            let deployments: Api<Deployment> = if is_cluster_wide_resources_allowed {
                Api::all(kube.clone())
            } else {
                Api::namespaced(kube.clone(), namespace)
            };
            for deployment in deployments.list(&list_params).await? {
                if deployment.status.and_then(|s| s.replicas).unwrap_or(0) == 0 {
                    if let (Some(namespace), Some(name)) = (deployment.metadata.namespace, deployment.metadata.name) {
                        let deployments: Api<Deployment> = Api::namespaced(kube.clone(), &namespace); // patch_scale needs to have deployments with namespace
                        deployments.patch_scale(&name, &patch_params, &patch).await?;
                    }
                }
            }
        }
        K8sResourceType::CronJob => {
            let (list_params, patch_params, patch) = get_patch_suspend(selector, false);
            let cron_jobs: Api<CronJob> = if is_cluster_wide_resources_allowed {
                Api::all(kube.clone())
            } else {
                Api::namespaced(kube.clone(), namespace)
            };
            for cron_job in cron_jobs.list(&list_params).await? {
                if let (Some(namespace), Some(name)) = (cron_job.metadata.namespace, cron_job.metadata.name) {
                    let cron_jobs: Api<CronJob> = Api::namespaced(kube.clone(), &namespace); // patch needs to have cron_jobs with namespace
                    cron_jobs.patch(&name, &patch_params, &patch).await?;
                }
            }
        }
        K8sResourceType::DaemonSet => {}
        K8sResourceType::Job => {}
    }

    Ok(())
}

fn get_patch_merge(selector: &str, desired_size: usize) -> (ListParams, PatchParams, Patch<Scale>) {
    let list_params = ListParams::default().labels(selector);
    let patch_params = PatchParams::default();
    let new_scale = Scale {
        metadata: Default::default(),
        spec: Some(ScaleSpec {
            replicas: Some(desired_size as i32),
        }),
        status: None,
    };
    let patch = Patch::Merge(new_scale);
    (list_params, patch_params, patch)
}

fn get_patch_suspend(selector: &str, desired_suspend_value: bool) -> (ListParams, PatchParams, Patch<Scale>) {
    let list_params = ListParams::default().labels(selector);
    let patch_params = PatchParams::default();
    let json_patch = json_patch::Patch(vec![json_patch::PatchOperation::Replace(json_patch::ReplaceOperation {
        path: Pointer::new(["spec", "suspend"]),
        value: Value::Bool(desired_suspend_value),
    })]);
    let patch = Patch::Json(json_patch);
    (list_params, patch_params, patch)
}

async fn wait_for_pods_to_be_in_correct_state(
    kube: &Client,
    namespace: &str,
    desired_size: usize,
    is_cluster_wide_resources_allowed: bool,
    list_params: &ListParams,
) {
    // Wait for pod to be destroyed/correctly scaled
    // Checking for readyness is not enough, as when downscaling pods in terminating are not listed in (ready_)replicas
    let pods: Api<Pod> = if is_cluster_wide_resources_allowed {
        Api::all(kube.clone())
    } else {
        Api::namespaced(kube.clone(), namespace)
    };
    while let Ok(pod) = pods.list(list_params).await {
        if pod.items.len() == desired_size {
            break;
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

pub struct PauseServiceAction {
    selector: String,
    k8s_resource_type: K8sResourceType,
    event_details: EventDetails,
    timeout: Duration,
    is_cluster_wide_resources_allowed: bool,
}

impl PauseServiceAction {
    pub fn new(
        selector: String,
        is_stateful: bool,
        timeout: Duration,
        event_details: EventDetails,
    ) -> PauseServiceAction {
        PauseServiceAction {
            selector,
            k8s_resource_type: if is_stateful {
                K8sResourceType::StateFulSet
            } else {
                K8sResourceType::Deployment
            },
            timeout,
            event_details,
            is_cluster_wide_resources_allowed: false,
        }
    }

    pub fn new_with_resource_type(
        selector: String,
        k8s_resource_type: K8sResourceType,
        timeout: Duration,
        event_details: EventDetails,
        is_cluster_wide_resources_allowed: bool,
    ) -> PauseServiceAction {
        PauseServiceAction {
            selector,
            k8s_resource_type,
            timeout,
            event_details,
            is_cluster_wide_resources_allowed,
        }
    }

    pub fn unpause_if_needed(&self, target: &DeploymentTarget) -> Result<(), Box<EngineError>> {
        let fut = unpause_service_if_needed(
            &target.kube,
            target.environment.namespace(),
            &self.selector,
            self.k8s_resource_type.clone(),
            self.is_cluster_wide_resources_allowed,
        );

        match block_on(async { tokio::time::timeout(self.timeout, fut).await }) {
            // Happy path
            Ok(Ok(())) => {}

            // error during scaling
            Ok(Err(kube_err)) => {
                let command_error = CommandError::new_from_safe_message(kube_err.to_string());
                return Err(Box::new(EngineError::new_k8s_scale_replicas(
                    self.event_details.clone(),
                    self.selector.clone(),
                    target.environment.namespace().to_string(),
                    0,
                    command_error,
                )));
            }
            // timeout
            Err(_) => {
                let command_error = CommandError::new_from_safe_message(format!(
                    "Timeout of {}s exceeded while un-pausing service",
                    self.timeout.as_secs()
                ));
                return Err(Box::new(EngineError::new_k8s_scale_replicas(
                    self.event_details.clone(),
                    self.selector.clone(),
                    target.environment.namespace().to_string(),
                    0,
                    command_error,
                )));
            }
        }

        Ok(())
    }
}

impl DeploymentAction for PauseServiceAction {
    fn on_create(&self, _target: &DeploymentTarget) -> Result<(), Box<EngineError>> {
        Ok(())
    }

    fn on_pause(&self, target: &DeploymentTarget) -> Result<(), Box<EngineError>> {
        let fut = pause_service(
            &target.kube,
            target.environment.namespace(),
            &self.selector,
            0,
            self.k8s_resource_type.clone(),
            self.is_cluster_wide_resources_allowed,
        );

        // Async block is necessary because tokio::time::timeout require a living tokio runtime, which does not exist
        // outside of the block_on. So must wrap it in an async task that will be exec inside the block_on
        let ret = block_on(async { tokio::time::timeout(self.timeout, fut).await });

        match ret {
            // Happy path
            Ok(Ok(())) => {}

            // error during scaling
            Ok(Err(kube_err)) => {
                let command_error = CommandError::new_from_safe_message(kube_err.to_string());
                return Err(Box::new(EngineError::new_k8s_scale_replicas(
                    self.event_details.clone(),
                    self.selector.clone(),
                    target.environment.namespace().to_string(),
                    0,
                    command_error,
                )));
            }
            // timeout
            Err(_) => {
                let command_error = CommandError::new_from_safe_message(format!(
                    "Timeout of {}s exceeded while scaling down service",
                    self.timeout.as_secs()
                ));
                return Err(Box::new(EngineError::new_k8s_scale_replicas(
                    self.event_details.clone(),
                    self.selector.clone(),
                    target.environment.namespace().to_string(),
                    0,
                    command_error,
                )));
            }
        }

        Ok(())
    }

    fn on_delete(&self, _target: &DeploymentTarget) -> Result<(), Box<EngineError>> {
        Ok(())
    }

    fn on_restart(&self, _target: &DeploymentTarget) -> Result<(), Box<EngineError>> {
        Ok(())
    }
}

#[cfg(feature = "test-local-kube")]
#[cfg(test)]
mod tests {
    use crate::deployment_action::pause_service::{
        has_cron_job_suspended_value, has_deployment_ready_replicas, has_statefulset_ready_replicas, pause_service,
        unpause_service_if_needed, K8sResourceType,
    };
    use crate::deployment_action::test_utils::{
        get_simple_cron_job, get_simple_deployment, get_simple_hpa, get_simple_statefulset, NamespaceForTest,
    };
    use function_name::named;
    use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
    use k8s_openapi::api::autoscaling::v1::HorizontalPodAutoscaler;
    use k8s_openapi::api::batch::v1::CronJob;
    use kube::api::PostParams;
    use kube::runtime::wait::await_condition;
    use kube::Api;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test(flavor = "multi_thread")]
    #[named]
    async fn test_scale_deployment() -> Result<(), Box<dyn std::error::Error>> {
        let kube_client = kube::Client::try_default().await.unwrap();
        let namespace = format!(
            "{}-{:?}",
            function_name!().replace('_', "-"),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
        );
        let timeout = Duration::from_secs(30);
        let deployments: Api<Deployment> = Api::namespaced(kube_client.clone(), &namespace);
        let deployment: Deployment = get_simple_deployment();
        let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(kube_client.clone(), &namespace);
        let hpa = get_simple_hpa();

        let app_name = deployment.metadata.name.clone().unwrap_or_default();
        let selector = format!("app={app_name}");

        // create simple deployment and wait for it to be ready
        let _ns = NamespaceForTest::new(kube_client.clone(), namespace.to_string()).await?;

        hpas.create(&PostParams::default(), &hpa).await.unwrap();
        deployments.create(&PostParams::default(), &deployment).await.unwrap();
        tokio::time::timeout(
            timeout,
            await_condition(deployments.clone(), &app_name, has_deployment_ready_replicas(1)),
        )
        .await??;

        // Scaling a service that does not exist should not fail
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, "app=totototo", 0, K8sResourceType::Deployment, false),
        )
        .await??;

        // Try to scale down our deployment
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 0, K8sResourceType::Deployment, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(deployments.clone(), &app_name, has_deployment_ready_replicas(0)),
        )
        .await??;

        // Try to scale up our deployment
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 1, K8sResourceType::Deployment, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(deployments.clone(), &app_name, has_deployment_ready_replicas(1)),
        )
        .await??;

        drop(_ns);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[named]
    async fn test_scale_statefulset() -> Result<(), Box<dyn std::error::Error>> {
        let kube_client = kube::Client::try_default().await.unwrap();
        let namespace = format!(
            "{}-{:?}",
            function_name!().replace('_', "-"),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
        );
        let timeout = Duration::from_secs(30);
        let statefulsets: Api<StatefulSet> = Api::namespaced(kube_client.clone(), &namespace);
        let statefulset: StatefulSet = get_simple_statefulset();
        let app_name = statefulset.metadata.name.clone().unwrap_or_default();
        let selector = format!("app={app_name}");

        // create simple deployment and wait for it to be ready
        let _ns = NamespaceForTest::new(kube_client.clone(), namespace.to_string()).await?;

        statefulsets.create(&PostParams::default(), &statefulset).await.unwrap();
        tokio::time::timeout(
            timeout,
            await_condition(statefulsets.clone(), &app_name, has_statefulset_ready_replicas(1)),
        )
        .await??;

        // Scaling a service that does not exist should not fail
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, "app=totototo", 0, K8sResourceType::StateFulSet, false),
        )
        .await??;

        // Try to scale down our deployment
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 0, K8sResourceType::StateFulSet, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(statefulsets.clone(), &app_name, has_statefulset_ready_replicas(0)),
        )
        .await??;

        // Try to scale up our deployment
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 1, K8sResourceType::StateFulSet, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(statefulsets.clone(), &app_name, has_statefulset_ready_replicas(1)),
        )
        .await??;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[named]
    async fn test_scale_cron_job() -> Result<(), Box<dyn std::error::Error>> {
        let kube_client = kube::Client::try_default().await.unwrap();
        let namespace = format!(
            "{}-{:?}",
            function_name!().replace('_', "-"),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
        );
        let timeout = Duration::from_secs(30);
        let cron_jobs: Api<CronJob> = Api::namespaced(kube_client.clone(), &namespace);
        let cron_job: CronJob = get_simple_cron_job();
        let app_name = cron_job.metadata.name.clone().unwrap_or_default();
        let selector = format!("app={app_name}");

        // create simple cron job and wait for it to be ready
        let _ns = NamespaceForTest::new(kube_client.clone(), namespace.to_string()).await?;

        cron_jobs.create(&PostParams::default(), &cron_job).await.unwrap();
        tokio::time::timeout(
            timeout,
            await_condition(cron_jobs.clone(), &app_name, has_cron_job_suspended_value(false)),
        )
        .await??;

        // Scaling a cron job that does not exist should not fail
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, "app=totototo", 0, K8sResourceType::CronJob, false),
        )
        .await??;

        // Try to suspend our cron-job
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 0, K8sResourceType::CronJob, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(cron_jobs.clone(), &app_name, has_cron_job_suspended_value(true)),
        )
        .await??;

        // Try to resume our cron-job
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 1, K8sResourceType::CronJob, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(cron_jobs.clone(), &app_name, has_cron_job_suspended_value(false)),
        )
        .await??;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[named]
    async fn test_unpause_deployment() -> Result<(), Box<dyn std::error::Error>> {
        let kube_client = kube::Client::try_default().await.unwrap();
        let namespace = format!(
            "{}-{:?}",
            function_name!().replace('_', "-"),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
        );
        let timeout = Duration::from_secs(30);
        let deployments: Api<Deployment> = Api::namespaced(kube_client.clone(), &namespace);
        let deployment: Deployment = get_simple_deployment();
        let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(kube_client.clone(), &namespace);
        let hpa = get_simple_hpa();

        let app_name = deployment.metadata.name.clone().unwrap_or_default();
        let selector = format!("app={app_name}");

        // create simple deployment and wait for it to be ready
        let _ns = NamespaceForTest::new(kube_client.clone(), namespace.to_string()).await?;

        hpas.create(&PostParams::default(), &hpa).await.unwrap();
        deployments.create(&PostParams::default(), &deployment).await.unwrap();
        tokio::time::timeout(
            timeout,
            await_condition(deployments.clone(), &app_name, has_deployment_ready_replicas(1)),
        )
        .await??;

        // Try to scale down our deployment
        tokio::time::timeout(
            timeout,
            pause_service(&kube_client, &namespace, &selector, 0, K8sResourceType::Deployment, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(deployments.clone(), &app_name, has_deployment_ready_replicas(0)),
        )
        .await??;

        tokio::time::timeout(
            timeout,
            unpause_service_if_needed(&kube_client, &namespace, &selector, K8sResourceType::Deployment, false),
        )
        .await??;
        tokio::time::timeout(
            timeout,
            await_condition(deployments.clone(), &app_name, has_deployment_ready_replicas(1)),
        )
        .await??;

        Ok(())
    }
}
