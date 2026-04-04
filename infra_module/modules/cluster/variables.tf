variable "compartment_ocid" {
  type        = string
  description = "Compartment OCID"
}

variable "kubernetes_version" {
  type        = string
  description = "Kubernetes version"
}

variable "cluster_name" {
  type        = string
  description = "Cluster name"
}

variable "vcn_id" {
  type        = string
  description = "VCN ID"
}

variable "api_subnet_id" {
  type        = string
  description = "API subnet ID"
}