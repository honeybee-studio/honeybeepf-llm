variable "compartment_ocid" {
  type        = string
  description = "Compartment OCID"
}

variable "cluster_id" {
  type        = string
  description = "OKE cluster ID"
}

variable "kubernetes_version" {
  type        = string
  description = "Kubernetes version"
}

variable "name_prefix" {
  type        = string
  description = "Name prefix"
}

variable "node_pool_size" {
  type        = number
  description = "Node pool size"
}

variable "node_shape" {
  type        = string
  description = "Node shape"
}

variable "node_ocpus" {
  type        = number
  description = "OCPUs for flexible shapes"
}

variable "node_memory_gbs" {
  type        = number
  description = "Memory in GBs for flexible shapes"
}

variable "node_subnet_id" {
  type        = string
  description = "Node subnet ID"
}

variable "ssh_public_key" {
  type        = string
  description = "SSH public key"
}
