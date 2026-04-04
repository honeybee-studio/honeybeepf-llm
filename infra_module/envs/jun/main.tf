terraform {
  required_version = ">= 1.3.0"
  required_providers {
    oci = {
      source  = "oracle/oci"
      version = ">= 4.0.0"
    }
  }
}

provider "oci" {}

module "stack" {
  source = "../../stack"

  compartment_ocid   = var.compartment_ocid
  tenancy_ocid       = var.tenancy_ocid
  bastion_client_cidr_list = var.bastion_client_cidr_list
  ssh_public_key     = var.ssh_public_key
  name_prefix        = var.name_prefix
  cluster_name       = var.cluster_name
  kubernetes_version = var.kubernetes_version
  node_pool_size     = var.node_pool_size
  node_shape         = var.node_shape
  node_ocpus         = var.node_ocpus
  node_memory_gbs    = var.node_memory_gbs

  vcn_cidr         = var.vcn_cidr
  api_subnet_cidr  = var.api_subnet_cidr
  node_subnet_cidr = var.node_subnet_cidr
}

output "cluster_id" {
  value = module.stack.cluster_id
}

output "kubeconfig_command" {
  value = module.stack.kubeconfig_command
}
