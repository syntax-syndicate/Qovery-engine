use qovery_engine::cloud_provider::kubernetes::KubernetesVersion;

pub const ON_PREMISE_KUBERNETES_VERSION: KubernetesVersion = KubernetesVersion::V1_28 {
    prefix: None,
    patch: None,
    suffix: None,
};
