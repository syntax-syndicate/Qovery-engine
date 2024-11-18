use crate::cloud_provider::gcp::kubernetes::Gke;
use crate::cloud_provider::kubeconfig_helper::update_kubeconfig_file;
use crate::cloud_provider::kubectl_utils::check_workers_on_create;
use crate::cloud_provider::kubernetes::Kubernetes;
use crate::cloud_provider::vault::{ClusterSecrets, ClusterSecretsGcp};
use crate::engine::InfrastructureContext;
use crate::errors::{CommandError, EngineError};
use crate::events::Stage::Infrastructure;
use crate::events::{EventDetails, EventMessage, InfrastructureStep};
use crate::infrastructure_action::deploy_helms::{HelmInfraContext, HelmInfraResources};
use crate::infrastructure_action::deploy_terraform::TerraformInfraResources;
use crate::infrastructure_action::gke::helm_charts::GkeHelmsDeployment;
use crate::infrastructure_action::gke::GkeQoveryTerraformOutput;
use crate::infrastructure_action::{InfraLogger, ToInfraTeraContext};
use crate::object_storage::ObjectStorage;
use crate::utilities::envs_to_string;
use base64::Engine;
use std::fs;
use std::path::PathBuf;

pub(super) fn create_gke_cluster(
    cluster: &Gke,
    infra_ctx: &InfrastructureContext,
    logger: impl InfraLogger,
) -> Result<(), Box<EngineError>> {
    let event_details = cluster.get_event_details(Infrastructure(InfrastructureStep::Create));
    logger.info("Preparing GKE cluster deployment.");

    let temp_dir = cluster.temp_dir();
    logger.info("Deploying GKE cluster.");
    if let Err(err) = create_object_storage(cluster, &logger, event_details.clone()) {
        logger.error(*err.clone(), None::<&str>);
        return Err(err);
    }

    // Terraform deployment dedicated to cloud resources
    let tera_context = cluster.to_infra_tera_context(infra_ctx)?;
    let tf_resources = TerraformInfraResources::new(
        tera_context.clone(),
        cluster.template_directory.join("terraform"),
        temp_dir.join("terraform"),
        event_details.clone(),
        envs_to_string(infra_ctx.cloud_provider().credentials_environment_variables()),
        cluster.context().is_dry_run_deploy(),
    );
    let qovery_terraform_output: GkeQoveryTerraformOutput = tf_resources.create(&logger)?;

    if cluster.context().is_dry_run_deploy() {
        logger.warn("Exiting. Dry run is not supported after the terraform action for now");
        return Ok(());
    }

    update_kubeconfig_file(cluster, &qovery_terraform_output.kubeconfig)?;

    // Configure kubectl to be able to connect to cluster
    let _ = cluster.configure_gcloud_for_cluster(infra_ctx); // TODO(ENG-1802): properly handle this error

    // Ensure all nodes are ready on Kubernetes
    check_workers_on_create(cluster, infra_ctx.cloud_provider(), None)
        .map_err(|e| Box::new(EngineError::new_k8s_node_not_ready(event_details.clone(), e)))?;
    logger.info("Kubernetes nodes have been successfully created");

    // Update cluster config to vault
    let kubeconfig = fs::read_to_string(cluster.kubeconfig_local_file_path()).map_err(|e| {
        Box::new(EngineError::new_cannot_retrieve_cluster_config_file(
            event_details.clone(),
            CommandError::new_from_safe_message(format!("Cannot read kubeconfig file: {e}",)),
        ))
    })?;

    let cluster_secrets = ClusterSecrets::new_google_gke(ClusterSecretsGcp::new(
        cluster.options.gcp_json_credentials.clone().into(),
        cluster.options.gcp_json_credentials.project_id.to_string(),
        cluster.region.clone(),
        Some(base64::engine::general_purpose::STANDARD.encode(kubeconfig)),
        Some(qovery_terraform_output.gke_cluster_public_hostname.clone()),
        cluster.kind(),
        infra_ctx.cloud_provider().name().to_string(),
        cluster.long_id().to_string(),
        cluster.options.grafana_admin_user.clone(),
        cluster.options.grafana_admin_password.clone(),
        infra_ctx.cloud_provider().organization_long_id().to_string(),
        cluster.context().is_test_cluster(),
    ));

    // vault config is not blocking
    let _ = cluster
        .update_gke_vault_config(event_details.clone(), cluster_secrets)
        .inspect_err(|e| {
            logger.warn(EventMessage::new(
                "Cannot push cluster config to Vault".to_string(),
                Some(e.to_string()),
            ))
        });

    let helms_deployments = GkeHelmsDeployment::new(
        HelmInfraContext::new(
            tera_context,
            PathBuf::from(infra_ctx.context().lib_root_dir()),
            cluster.template_directory.clone(),
            cluster.temp_dir().join("helms"),
            event_details.clone(),
            vec![],
            cluster.context().is_dry_run_deploy(),
        ),
        qovery_terraform_output,
        cluster,
    );
    helms_deployments.deploy_charts(infra_ctx, &logger)?;

    Ok(())
}

fn create_object_storage(
    cluster: &Gke,
    logger: &impl InfraLogger,
    event_details: EventDetails,
) -> Result<(), Box<EngineError>> {
    logger.info("Create Qovery managed object storage buckets.");
    for bucket_name in &[&cluster.logs_bucket_name()] {
        let existing_bucket = cluster
            .object_storage
            .create_bucket(bucket_name, cluster.advanced_settings.resource_ttl(), true)
            .map_err(|e| Box::new(EngineError::new_object_storage_error(event_details.clone(), e)))?;

        logger.info(format!("Object storage bucket {} already exists", &bucket_name));
        // Update set versioning to true if not activated on the bucket (bucket created before this option was enabled)
        // This can be removed at some point in the future, just here to handle legacy GCP buckets
        // TODO(ENG-1736): remove this update once all existing buckets have versioning activated
        if existing_bucket.versioning_activated {
            continue;
        }

        if let Err(err) = cluster.object_storage.update_bucket(bucket_name, true) {
            let error = EngineError::new_object_storage_error(event_details.clone(), err);
            return Err(Box::new(error));
        }
    }
    Ok(())
}
