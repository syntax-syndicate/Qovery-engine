use crate::cloud_provider::aws::kubernetes::eks::EKS;
use crate::cloud_provider::kubernetes::Kubernetes;
use crate::cloud_provider::models::{KubernetesClusterAction, NodeGroupsFormat};
use crate::engine::InfrastructureContext;
use crate::errors::EngineError;
use crate::events::{InfrastructureStep, Stage};
use crate::infrastructure_action::deploy_terraform::TerraformInfraResources;
use crate::infrastructure_action::eks::karpenter::Karpenter;
use crate::infrastructure_action::eks::nodegroup::should_update_desired_nodes;
use crate::infrastructure_action::eks::tera_context::eks_tera_context;
use crate::infrastructure_action::eks::utils::{define_cluster_upgrade_timeout, get_rusoto_eks_client};
use crate::infrastructure_action::InfraLogger;
use crate::runtime::block_on;
use crate::services::kube_client::SelectK8sResourceBy;
use crate::utilities::envs_to_string;
use std::path::PathBuf;

pub fn pause_eks_cluster(
    kubernetes: &EKS,
    infra_ctx: &InfrastructureContext,
    logger: impl InfraLogger,
) -> Result<(), Box<EngineError>> {
    logger.info("Pausing cluster deployment.");

    // For Karpenter
    let kube_client = infra_ctx.mk_kube_client()?;
    if kubernetes.is_karpenter_enabled() {
        block_on(Karpenter::pause(kubernetes, infra_ctx.cloud_provider(), &kube_client))?;
        logger.info(format!("Kubernetes cluster {} successfully paused", kubernetes.name()));
        return Ok(());
    }

    // Legacy flow, that manage node groups
    let event_details = kubernetes.get_event_details(Stage::Infrastructure(InfrastructureStep::Pause));
    let aws_eks_client = match get_rusoto_eks_client(event_details.clone(), kubernetes, infra_ctx.cloud_provider()) {
        Ok(value) => Some(value),
        Err(_) => None,
    };

    let node_groups_with_desired_states = should_update_desired_nodes(
        event_details.clone(),
        kubernetes,
        KubernetesClusterAction::Pause,
        &kubernetes.nodes_groups,
        aws_eks_client,
    )?;

    // in case error, this should not be a blocking error
    let pods_list = block_on(kube_client.get_pods(event_details.clone(), None, SelectK8sResourceBy::All))
        .unwrap_or(Vec::with_capacity(0));

    let (timeout, message) = define_cluster_upgrade_timeout(pods_list, KubernetesClusterAction::Pause);
    let cluster_upgrade_timeout_in_min = timeout;
    if let Some(x) = message {
        logger.info(x);
    }

    // generate terraform files and copy them into temp dir
    let mut tera_context = eks_tera_context(
        kubernetes,
        infra_ctx.cloud_provider(),
        infra_ctx.dns_provider(),
        &kubernetes.zones,
        &node_groups_with_desired_states,
        &kubernetes.options,
        cluster_upgrade_timeout_in_min,
        false,
        kubernetes.advanced_settings(),
        kubernetes.qovery_allowed_public_access_cidrs.as_ref(),
    )?;

    // pause: remove all worker nodes to reduce the bill but keep master to keep all the deployment config, certificates etc...
    tera_context.insert("eks_worker_nodes", &Vec::<NodeGroupsFormat>::new());

    let tf_action = TerraformInfraResources::new(
        tera_context.clone(),
        PathBuf::from(&kubernetes.template_directory).join("terraform"),
        kubernetes.temp_dir().join("terraform"),
        event_details.clone(),
        envs_to_string(infra_ctx.cloud_provider().credentials_environment_variables()),
        kubernetes.context().is_dry_run_deploy(),
    );
    tf_action.pause(&["aws_eks_node_group."])?;

    logger.info(format!("Kubernetes cluster {} successfully paused", kubernetes.name()));
    Ok(())
}