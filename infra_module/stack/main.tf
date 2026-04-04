module "network" {
  source            = "../modules/network"
  compartment_ocid  = var.compartment_ocid
  tenancy_ocid      = var.tenancy_ocid
  bastion_client_cidr_list = var.bastion_client_cidr_list
  ssh_public_key    = var.ssh_public_key
  name_prefix       = var.name_prefix
  vcn_cidr          = var.vcn_cidr
  api_subnet_cidr   = var.api_subnet_cidr
  node_subnet_cidr  = var.node_subnet_cidr
}

module "cluster" {
  source              = "../modules/cluster"
  compartment_ocid    = var.compartment_ocid
  kubernetes_version  = var.kubernetes_version
  cluster_name        = var.cluster_name
  vcn_id              = module.network.vcn_id
  api_subnet_id       = module.network.api_subnet_id
}

module "nodepool" {
  source              = "../modules/nodepool"
  compartment_ocid    = var.compartment_ocid
  cluster_id          = module.cluster.cluster_id
  kubernetes_version  = var.kubernetes_version
  name_prefix         = var.name_prefix
  node_pool_size      = var.node_pool_size
  node_shape          = var.node_shape
  node_ocpus          = var.node_ocpus
  node_memory_gbs     = var.node_memory_gbs
  node_subnet_id      = module.network.node_subnet_id
  ssh_public_key      = var.ssh_public_key
}