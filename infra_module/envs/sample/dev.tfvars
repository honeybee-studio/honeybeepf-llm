# IMPORTANT: To use this tfvars file, you need to set the following env vars in your shell
#   put this under infra_module/envs/<your-name>/dev.tfvars
#   export TF_VAR_compartment_ocid="<your-compartment-ocid> or "<your-tenancy-ocid>"
#   export TF_VAR_tenancy_ocid="<your-tenancy-ocid>"
#   export TF_VAR_ssh_public_key="$(cat ~/.ssh/id_rsa.pub)"
#   # For list variables, export JSON
#   export TF_VAR_bastion_client_cidr_list="[\"$(curl -s ifconfig.me)/32\"]"

# Do NOT set these two in tfvars if you want env vars to take effect.

name_prefix        = "oke-<your-env-name>-dev"
cluster_name       = "free-tier-oke-<your-env-name>-dev"
kubernetes_version = "v1.34.1"

node_pool_size   = 2
node_shape       = "VM.Standard.A1.Flex"
node_ocpus       = 2
node_memory_gbs  = 15

# Network CIDRs (customize if you need isolation per developer)
name_prefix       = "oke"
vcn_cidr          = "10.0.0.0/16"
vpn_subnet_cidr   = "10.0.10.0/24"
api_subnet_cidr   = "10.0.0.0/28"
node_subnet_cidr  = "10.0.1.0/24"