output "cluster_id" {
  value = module.cluster.cluster_id
}

output "kubeconfig_command" {
  value = "oci ce cluster create-kubeconfig --cluster-id ${module.cluster.cluster_id} --file $HOME/.kube/config --token-version 2.0.0  --kube-endpoint PUBLIC_ENDPOINT"
}