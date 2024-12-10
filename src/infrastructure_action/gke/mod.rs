mod cluster_create;
mod cluster_delete;
mod cluster_pause;
mod cluster_upgrade;
mod helm_charts;
mod tera_context;

use crate::cloud_provider::gcp::kubernetes::Gke;
use crate::cloud_provider::kubernetes::{send_progress_on_long_task, KubernetesUpgradeStatus};
use crate::cloud_provider::service::Action;
use crate::engine::InfrastructureContext;
use crate::errors::EngineError;
use crate::events::InfrastructureStep;
use crate::infrastructure_action::gke::cluster_create::create_gke_cluster;
use crate::infrastructure_action::gke::cluster_delete::delete_gke_cluster;
use crate::infrastructure_action::gke::cluster_pause::pause_gke_cluster;
use crate::infrastructure_action::gke::cluster_upgrade::upgrade_gke_cluster;
use crate::infrastructure_action::InfrastructureAction;
use serde_derive::{Deserialize, Serialize};

impl InfrastructureAction for Gke {
    fn create_cluster(
        &self,
        infra_ctx: &InfrastructureContext,
        _has_been_upgraded: bool,
    ) -> Result<(), Box<EngineError>> {
        let logger = mk_logger(infra_ctx.kubernetes(), InfrastructureStep::Create);
        send_progress_on_long_task(self, Action::Create, || create_gke_cluster(self, infra_ctx, logger))
    }

    fn pause_cluster(&self, infra_ctx: &InfrastructureContext) -> Result<(), Box<EngineError>> {
        let logger = mk_logger(infra_ctx.kubernetes(), InfrastructureStep::Pause);
        send_progress_on_long_task(self, Action::Pause, || pause_gke_cluster(self, infra_ctx, logger))
    }

    fn delete_cluster(&self, infra_ctx: &InfrastructureContext) -> Result<(), Box<EngineError>> {
        let logger = mk_logger(infra_ctx.kubernetes(), InfrastructureStep::Delete);
        send_progress_on_long_task(self, Action::Delete, || delete_gke_cluster(self, infra_ctx, logger))
    }

    fn upgrade_cluster(
        &self,
        infra_ctx: &InfrastructureContext,
        kubernetes_upgrade_status: KubernetesUpgradeStatus,
    ) -> Result<(), Box<EngineError>> {
        let logger = mk_logger(infra_ctx.kubernetes(), InfrastructureStep::Upgrade);

        send_progress_on_long_task(self, Action::Create, || {
            upgrade_gke_cluster(self, infra_ctx, kubernetes_upgrade_status, logger)
        })
    }
}

use super::utils::{from_terraform_value, mk_logger};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GkeQoveryTerraformOutput {
    #[serde(deserialize_with = "from_terraform_value")]
    pub gke_cluster_public_hostname: String,
    #[serde(deserialize_with = "from_terraform_value")]
    #[serde(default)]
    pub loki_logging_service_account_email: String,
    #[serde(deserialize_with = "from_terraform_value")]
    pub kubeconfig: String,
}