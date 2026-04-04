variable "compartment_ocid" {
  description = "Target compartment OCID"
  type        = string
}

variable "tenancy_ocid" {
  description = "Target tenancy OCID"
  type        = string
}

variable "bastion_client_cidr_list" {
  description = "List of CIDRs allowed to connect to Bastion (your IP)"
  type        = list(string)
}

variable "ssh_public_key" {
  description = "SSH public key for worker nodes"
  type        = string
}

variable "name_prefix" {
  description = "Prefix for resource names"
  type        = string
  default     = "oke"
}

variable "cluster_name" {
  description = "OKE cluster name"
  type        = string
  default     = "free-tier-oke"
}

variable "kubernetes_version" {
  description = "Kubernetes version for OKE"
  type        = string
  default     = "v1.34.1"
}

variable "node_pool_size" {
  description = "Number of nodes in the node pool"
  type        = number
  default     = 2
}

variable "node_ocpus" {
  description = "vCPUs for flexible shapes"
  type        = number
  default     = 2
}

variable "node_memory_gbs" {
  description = "Memory (GB) for flexible shapes"
  type        = number
  default     = 12
}

variable "node_shape" {
  description = "Compute shape for worker nodes"
  type        = string
  default     = "VM.Standard.A1.Flex"
}

variable "vcn_cidr" {
  description = "CIDR for the VCN"
  type        = string
  default     = "10.0.0.0/16"
}

variable "api_subnet_cidr" {
  description = "CIDR for API (control plane) subnet"
  type        = string
  default     = "10.0.0.0/24"
}

variable "node_subnet_cidr" {
  description = "CIDR for node subnet"
  type        = string
  default     = "10.0.1.0/24"
}