use crate::cloud_provider::aws::kubernetes::{
    KarpenterNodePool, KarpenterNodePoolRequirement, KarpenterNodePoolRequirementKey, KarpenterParameters,
    KarpenterRequirementOperator, UserNetworkConfig,
};
use crate::cloud_provider::helm::{
    ChartInfo, ChartInstallationChecker, ChartSetValue, CommonChart, HelmAction, HelmChartError, HelmChartNamespaces,
};
use crate::cloud_provider::helm_charts::{
    HelmChartDirectoryLocation, HelmChartPath, HelmChartValuesFilePath, ToCommonHelmChart,
};
use crate::errors::CommandError;
use itertools::Itertools;
use kube::Client;

pub struct KarpenterConfigurationChart {
    chart_path: HelmChartPath,
    chart_values_path: HelmChartValuesFilePath,
    cluster_name: String,
    replace_cluster_autoscaler: bool,
    security_group_id: String,
    cluster_id: String,
    cluster_long_id: String,
    organization_id: String,
    organization_long_id: String,
    region: String,
    karpenter_parameters: Option<KarpenterParameters>,
    explicit_subnet_ids: Vec<String>,
    pleco_resources_ttl: i32,
}

impl KarpenterConfigurationChart {
    pub fn new(
        chart_prefix_path: Option<&str>,
        cluster_name: String,
        replace_cluster_autoscaler: bool,
        cluster_security_group_id: String,
        cluster_id: &str,
        cluster_long_id: uuid::Uuid,
        organization_id: &str,
        organization_long_id: uuid::Uuid,
        region: &str,
        karpenter_parameters: Option<KarpenterParameters>,
        user_network_config: Option<&UserNetworkConfig>,
        pleco_resources_ttl: i32,
    ) -> Self {
        KarpenterConfigurationChart {
            chart_path: HelmChartPath::new(
                chart_prefix_path,
                HelmChartDirectoryLocation::CloudProviderFolder,
                KarpenterConfigurationChart::chart_name(),
            ),
            chart_values_path: HelmChartValuesFilePath::new(
                chart_prefix_path,
                HelmChartDirectoryLocation::CloudProviderFolder,
                KarpenterConfigurationChart::chart_name(),
            ),
            cluster_name,
            replace_cluster_autoscaler,
            security_group_id: cluster_security_group_id,
            cluster_id: cluster_id.to_string(),
            cluster_long_id: cluster_long_id.to_string(),
            organization_id: organization_id.to_string(),
            organization_long_id: organization_long_id.to_string(),
            region: region.to_string(),
            karpenter_parameters,
            explicit_subnet_ids: if let Some(user_network_config) = &user_network_config {
                let combined_subnets = [
                    &user_network_config.eks_subnets_zone_a_ids,
                    &user_network_config.eks_subnets_zone_b_ids,
                    &user_network_config.eks_subnets_zone_c_ids,
                ]
                .iter()
                .flat_map(|v| v.iter())
                .cloned()
                .collect_vec();

                combined_subnets
            } else {
                vec![]
            },
            pleco_resources_ttl,
        }
    }

    pub fn chart_name() -> String {
        "karpenter-configuration".to_string()
    }

    fn get_karpenter_node_pools_requirements(
        spot_enabled: bool,
        qovery_node_pools: Option<KarpenterNodePool>,
    ) -> Vec<KarpenterNodePoolRequirement> {
        if let Some(pools) = qovery_node_pools {
            if let Some(mut requirements) = pools.requirements.filter(|req| !req.is_empty()) {
                requirements.push(KarpenterNodePoolRequirement {
                    key: KarpenterNodePoolRequirementKey::CapacityType,
                    operator: Some(KarpenterRequirementOperator::In),
                    values: if spot_enabled {
                        vec!["spot".to_string(), "on-demand".to_string()]
                    } else {
                        vec!["on-demand".to_string()]
                    },
                });
                return requirements;
            }
        }
        Self::default_karpenter_node_pools_requirements(spot_enabled)
    }

    fn default_karpenter_node_pools_requirements(spot_enabled: bool) -> Vec<KarpenterNodePoolRequirement> {
        vec![
            KarpenterNodePoolRequirement {
                key: KarpenterNodePoolRequirementKey::Arch,
                operator: Some(KarpenterRequirementOperator::In),
                values: vec!["amd64".to_string(), "arm64".to_string()],
            },
            KarpenterNodePoolRequirement {
                key: KarpenterNodePoolRequirementKey::Os,
                operator: Some(KarpenterRequirementOperator::In),
                values: vec!["linux".to_string()],
            },
            KarpenterNodePoolRequirement {
                key: KarpenterNodePoolRequirementKey::InstanceCategory,
                operator: Some(KarpenterRequirementOperator::In),
                values: vec![
                    "c".to_string(),
                    "d".to_string(),
                    "h".to_string(),
                    "i".to_string(),
                    "im".to_string(),
                    "inf".to_string(),
                    "is".to_string(),
                    "m".to_string(),
                    "r".to_string(),
                    "t".to_string(),
                    "trn".to_string(),
                    "x".to_string(),
                    "z".to_string(),
                ],
            },
            KarpenterNodePoolRequirement {
                key: KarpenterNodePoolRequirementKey::InstanceGeneration,
                operator: Some(KarpenterRequirementOperator::Gt),
                values: vec!["2".to_string()],
            },
            KarpenterNodePoolRequirement {
                key: KarpenterNodePoolRequirementKey::CapacityType,
                operator: Some(KarpenterRequirementOperator::In),
                values: match spot_enabled {
                    false => vec!["on-demand".to_string()],
                    true => vec!["spot".to_string(), "on-demand".to_string()],
                },
            },
        ]
    }
}

impl ToCommonHelmChart for KarpenterConfigurationChart {
    fn to_common_helm_chart(&self) -> Result<CommonChart, HelmChartError> {
        let (disk_size_in_gib, spot_enabled, qovery_node_pools) =
            if let Some(karpenter_parameters) = &self.karpenter_parameters {
                (
                    karpenter_parameters.disk_size_in_gib,
                    karpenter_parameters.spot_enabled,
                    karpenter_parameters.qovery_node_pools.clone(),
                )
            } else {
                (0, false, None)
            };

        let mut values = vec![
            ChartSetValue {
                key: "clusterName".to_string(),
                value: self.cluster_name.clone(),
            },
            ChartSetValue {
                key: "securityGroupId".to_string(),
                value: self.security_group_id.clone(),
            },
            ChartSetValue {
                key: "diskSizeInGib".to_string(),
                value: format!("{}Gi", disk_size_in_gib),
            },
            ChartSetValue {
                key: "tags.ClusterId".to_string(),
                value: self.cluster_id.clone(),
            },
            ChartSetValue {
                key: "tags.ClusterLongId".to_string(),
                value: self.cluster_long_id.clone(),
            },
            ChartSetValue {
                key: "tags.OrganizationId".to_string(),
                value: self.organization_id.clone(),
            },
            ChartSetValue {
                key: "tags.OrganizationLongId".to_string(),
                value: self.organization_long_id.clone(),
            },
            ChartSetValue {
                key: "tags.Region".to_string(),
                value: self.region.clone(),
            },
        ];

        if !self.explicit_subnet_ids.is_empty() {
            values.push(ChartSetValue {
                key: "explicitSubnetIds".to_string(),
                value: format!("{{{}}}", self.explicit_subnet_ids.join(",")),
            });
        }

        let karpenter_node_pools_requirements =
            Self::get_karpenter_node_pools_requirements(spot_enabled, qovery_node_pools);

        karpenter_node_pools_requirements
            .iter()
            .enumerate()
            .for_each(|(index, requirement)| {
                let prefix = format!("global_node_pools.requirements[{}]", index);

                let formated_values = if requirement.key == KarpenterNodePoolRequirementKey::Arch {
                    // The nodepool support only lowercase value for arch
                    requirement.values.iter().map(|value| value.to_lowercase()).join(",")
                } else {
                    requirement.values.join(",")
                };

                values.push(ChartSetValue {
                    key: format!("{}.key", prefix),
                    value: requirement.key.to_k8s_label(),
                });
                values.push(ChartSetValue {
                    key: format!("{}.operator", prefix),
                    value: requirement
                        .operator
                        .as_ref()
                        .unwrap_or(&KarpenterRequirementOperator::In)
                        .to_string(),
                });
                values.push(ChartSetValue {
                    key: format!("{}.values", prefix),
                    value: format!("{{{}}}", formated_values),
                });
            });

        let mut values_string: Vec<ChartSetValue> = vec![];
        if self.pleco_resources_ttl > 0 {
            values_string.push(ChartSetValue {
                key: "tags.ttl".to_string(),
                value: format!("\"{}\"", self.pleco_resources_ttl),
            });
        }

        Ok(CommonChart {
            chart_info: ChartInfo {
                name: KarpenterConfigurationChart::chart_name(),
                action: match self.replace_cluster_autoscaler {
                    true => HelmAction::Deploy,
                    false => HelmAction::Destroy,
                },
                namespace: HelmChartNamespaces::KubeSystem,
                path: self.chart_path.to_string(),
                values_files: vec![self.chart_values_path.to_string()],
                values,
                values_string,
                ..Default::default()
            },
            chart_installation_checker: Some(Box::new(KarpenterChartChecker::new())),
            vertical_pod_autoscaler: None, // enabled in the chart configuration
        })
    }
}

#[derive(Clone)]
pub struct KarpenterChartChecker {}

impl KarpenterChartChecker {
    pub fn new() -> KarpenterChartChecker {
        KarpenterChartChecker {}
    }
}

impl Default for KarpenterChartChecker {
    fn default() -> Self {
        KarpenterChartChecker::new()
    }
}

impl ChartInstallationChecker for KarpenterChartChecker {
    fn verify_installation(&self, _kube_client: &Client) -> Result<(), CommandError> {
        // TODO(ENG-1366): Implement chart install verification
        Ok(())
    }

    fn clone_dyn(&self) -> Box<dyn ChartInstallationChecker> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;
    use serde::Deserialize;
    use serde_yaml::{self, Value};
    use std::env;
    use uuid::Uuid;

    use crate::cloud_provider::aws::kubernetes::{
        KarpenterNodePool, KarpenterNodePoolRequirement, KarpenterNodePoolRequirementKey, KarpenterParameters,
        KarpenterRequirementOperator,
    };
    use crate::cloud_provider::helm_charts::{
        get_helm_path_kubernetes_provider_sub_folder_name, get_helm_values_set_in_code_but_absent_in_values_file,
        HelmChartType, ToCommonHelmChart,
    };
    use crate::cloud_provider::kubernetes::Kind as KubernetesKind;
    use crate::cloud_provider::models::CpuArchitecture::ARM64;
    use crate::cmd::helm::Helm;
    use crate::infrastructure_action::eks::helm_charts::karpenter_configuration::KarpenterConfigurationChart;

    /// Makes sure chart directory containing all YAML files exists.
    #[test]
    fn karpenter_configuration_chart_directory_exists_test() {
        // setup:
        let chart = create_chart(true, None);

        let current_directory = env::current_dir().expect("Impossible to get current directory");
        let chart_path = format!(
            "{}/lib/{}/bootstrap/charts/{}/Chart.yaml",
            current_directory
                .to_str()
                .expect("Impossible to convert current directory to string"),
            get_helm_path_kubernetes_provider_sub_folder_name(
                chart.chart_path.helm_path(),
                HelmChartType::CloudProviderSpecific(KubernetesKind::Eks)
            ),
            KarpenterConfigurationChart::chart_name(),
        );

        // execute
        let values_file = std::fs::File::open(&chart_path);

        // verify:
        assert!(values_file.is_ok(), "Chart directory should exist: `{chart_path}`");
    }

    /// Makes sure chart values file exists.
    #[test]
    fn karpenter_configuration_chart_values_file_exists_test() {
        // setup:
        let chart = create_chart(true, None);

        let current_directory = env::current_dir().expect("Impossible to get current directory");
        let chart_values_path = format!(
            "{}/lib/{}/bootstrap/chart_values/{}.yaml",
            current_directory
                .to_str()
                .expect("Impossible to convert current directory to string"),
            get_helm_path_kubernetes_provider_sub_folder_name(
                chart.chart_values_path.helm_path(),
                HelmChartType::CloudProviderSpecific(KubernetesKind::Eks)
            ),
            KarpenterConfigurationChart::chart_name(),
        );

        // execute
        let values_file = std::fs::File::open(&chart_values_path);

        // verify:
        assert!(values_file.is_ok(), "Chart values file should exist: `{chart_values_path}`");
    }

    /// Make sure rust code doesn't set a value not declared inside values file.
    /// All values should be declared / set in values file unless it needs to be injected via rust code.
    #[test]
    fn karpenter_configuration_rust_overridden_values_exists_in_values_yaml_test() {
        // setup:
        let chart = create_chart(false, None);
        let common_chart = chart.to_common_helm_chart().unwrap();

        // execute:
        let missing_fields = get_helm_values_set_in_code_but_absent_in_values_file(
            common_chart,
            format!(
                "/lib/{}/bootstrap/chart_values/{}.yaml",
                get_helm_path_kubernetes_provider_sub_folder_name(
                    chart.chart_values_path.helm_path(),
                    HelmChartType::CloudProviderSpecific(KubernetesKind::Eks)
                ),
                KarpenterConfigurationChart::chart_name()
            ),
        );

        // verify:
        assert!(missing_fields.is_none(), "Some fields are missing in values file, add those (make sure they still exist in chart values), fields: {}", missing_fields.unwrap_or_default().join(","));
    }

    #[test]
    fn test_karpenter_configuration() {
        // Define your test cases
        let test_cases = vec![
            TestCase {
                with_spot: true,
                qovery_node_pools: None,
                verify_fn: verify_default_node_pools,
            },
            TestCase {
                with_spot: false,
                qovery_node_pools: None,
                verify_fn: verify_default_node_pools,
            },
            TestCase {
                with_spot: false,
                qovery_node_pools: Some(KarpenterNodePool {
                    requirements: Some(vec![
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::InstanceCategory,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["c".to_string()],
                        },
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::Arch,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["AMD64".to_string()],
                        },
                    ]),
                }),
                verify_fn: verify_custom_node_pools,
            },
            TestCase {
                with_spot: true,
                qovery_node_pools: Some(KarpenterNodePool {
                    requirements: Some(vec![
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::InstanceCategory,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["c".to_string()],
                        },
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::Arch,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["AMD64".to_string()],
                        },
                    ]),
                }),
                verify_fn: verify_custom_node_pools,
            },
            TestCase {
                with_spot: false,
                qovery_node_pools: Some(KarpenterNodePool {
                    requirements: Some(vec![
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::InstanceCategory,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["c".to_string()],
                        },
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::Arch,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["AMD64".to_string()],
                        },
                        KarpenterNodePoolRequirement {
                            key: KarpenterNodePoolRequirementKey::CapacityType,
                            operator: Some(KarpenterRequirementOperator::In),
                            values: vec!["spot".to_string()],
                        },
                    ]),
                }),
                verify_fn: verify_custom_node_pools,
            },
        ];

        // Iterate through each test case
        for test_case in test_cases {
            let yaml = generate_chart_yaml(test_case.with_spot, test_case.qovery_node_pools.clone());
            (test_case.verify_fn)(&yaml, test_case.with_spot);
        }
    }

    struct TestCase {
        with_spot: bool,
        qovery_node_pools: Option<KarpenterNodePool>,
        verify_fn: fn(&str, bool),
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Requirement {
        key: String,
        operator: String,
        values: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct SpecT {
        requirements: Vec<Requirement>,
    }

    #[derive(Debug, Deserialize)]
    struct Template {
        spec: SpecT,
    }

    #[derive(Debug, Deserialize)]
    struct Spec {
        template: Template,
    }

    #[derive(Debug, Deserialize)]
    struct Metadata {
        name: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    struct NodePool {
        // apiVersion: String,
        kind: String,
        spec: Spec,
        metadata: Metadata,
    }

    fn create_chart(with_spot: bool, qovery_node_pools: Option<KarpenterNodePool>) -> KarpenterConfigurationChart {
        KarpenterConfigurationChart::new(
            None,
            "whatever".to_string(),
            true,
            "security_group".to_string(),
            "cluster_id",
            Uuid::new_v4(),
            "organization_id",
            Uuid::new_v4(),
            "region",
            Some(KarpenterParameters {
                spot_enabled: with_spot,
                max_node_drain_time_in_secs: None,
                disk_size_in_gib: 50,
                default_service_architecture: ARM64,
                qovery_node_pools,
            }),
            None,
            0,
        )
    }

    fn generate_chart_yaml(with_spot: bool, qovery_node_pools: Option<KarpenterNodePool>) -> String {
        // setup:
        let chart = create_chart(with_spot, qovery_node_pools);

        let current_directory = env::current_dir().expect("Impossible to get current directory");
        let chart_path = format!(
            "{}/lib/{}/bootstrap/charts/{}",
            current_directory
                .to_str()
                .expect("Impossible to convert current directory to string"),
            get_helm_path_kubernetes_provider_sub_folder_name(
                chart.chart_path.helm_path(),
                HelmChartType::CloudProviderSpecific(KubernetesKind::Eks)
            ),
            KarpenterConfigurationChart::chart_name(),
        );

        let helm = Helm::new::<String>(None, &[]).expect("Failed to initialize Helm");
        let common_chart = chart.to_common_helm_chart().expect("Failed to convert to common chart");

        // execute
        helm.get_template(&chart_path, &common_chart.chart_info)
            .expect("Failed to get Helm template")
    }

    fn verify_default_node_pools(yaml: &str, with_spot: bool) {
        let deserializer = serde_yaml::Deserializer::from_str(yaml);

        let node_pools: Vec<_> = deserializer
            .map(|document| {
                let value: Value = Value::deserialize(document).expect("Failed to deserialize YAML document");
                serde_yaml::from_value::<NodePool>(value)
            })
            .filter_map(Result::ok)
            .collect();

        assert_eq!(node_pools.len(), 2, "Expected exactly 2 node pools");
        assert_eq!(
            node_pools
                .iter()
                .map(|node_pool| node_pool.metadata.name.clone())
                .collect_vec(),
            vec!["default".to_string(), "stable".to_string()],
            "Expected default and stable"
        );
        for node_pool in node_pools {
            assert_eq!(node_pool.kind, "NodePool");
            let reqs = &node_pool.spec.template.spec.requirements;
            assert_eq!(reqs.len(), 5, "Expected exactly 5 requirements");

            assert_requirement_exists(reqs, "kubernetes.io/arch", "In", vec!["amd64".to_string(), "arm64".to_string()]);
            assert_requirement_exists(reqs, "kubernetes.io/os", "In", vec!["linux".to_string()]);

            assert_requirement_exists(
                reqs,
                "karpenter.sh/capacity-type",
                "In",
                if with_spot {
                    vec!["spot".to_string(), "on-demand".to_string()]
                } else {
                    vec!["on-demand".to_string()]
                },
            );

            assert_requirement_exists(
                reqs,
                "karpenter.k8s.aws/instance-category",
                "In",
                vec![
                    "c".to_string(),
                    "d".to_string(),
                    "h".to_string(),
                    "i".to_string(),
                    "im".to_string(),
                    "inf".to_string(),
                    "is".to_string(),
                    "m".to_string(),
                    "r".to_string(),
                    "t".to_string(),
                    "trn".to_string(),
                    "x".to_string(),
                    "z".to_string(),
                ],
            );

            assert_requirement_exists(reqs, "karpenter.k8s.aws/instance-generation", "Gt", vec!["2".to_string()]);
        }
    }

    fn verify_custom_node_pools(yaml: &str, with_spot: bool) {
        let deserializer = serde_yaml::Deserializer::from_str(yaml);

        let node_pools: Vec<_> = deserializer
            .map(|document| {
                let value: Value = Value::deserialize(document).expect("Failed to deserialize YAML document");
                serde_yaml::from_value::<NodePool>(value)
            })
            .filter_map(Result::ok)
            .collect();

        assert_eq!(node_pools.len(), 2, "Expected exactly 2 node pools");
        assert_eq!(
            node_pools
                .iter()
                .map(|node_pool| node_pool.metadata.name.clone())
                .collect_vec(),
            vec!["default".to_string(), "stable".to_string()],
            "Expected default and stable"
        );
        for node_pool in node_pools {
            assert_eq!(node_pool.kind, "NodePool");
            let reqs = &node_pool.spec.template.spec.requirements;

            assert_requirement_exists(reqs, "karpenter.k8s.aws/instance-category", "In", vec!["c".to_string()]);
            assert_requirement_exists(reqs, "kubernetes.io/arch", "In", vec!["amd64".to_string()]);
            assert_requirement_exists(
                reqs,
                "karpenter.sh/capacity-type",
                "In",
                if with_spot {
                    vec!["spot".to_string(), "on-demand".to_string()]
                } else {
                    vec!["on-demand".to_string()]
                },
            );
        }
    }

    fn assert_requirement_exists(reqs: &[Requirement], key: &str, operator: &str, values: Vec<String>) {
        assert!(
            reqs.contains(&Requirement {
                key: key.to_string(),
                operator: operator.to_string(),
                values,
            }),
            "Expected {} requirement to be present",
            key
        );
    }
}