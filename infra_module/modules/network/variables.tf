variable "compartment_ocid" {
  description = "OCI Compartment OCID"
  type        = string
}

variable "tenancy_ocid" {
  description = "OCI Tenancy OCID"
  type        = string
}

variable "bastion_client_cidr_list" {
  description = "CIDR allow list for Bastion client access"
  type        = list(string)
}

variable "ssh_public_key" {
  description = "SSH public key for worker nodes"
  type        = string
}

variable "name_prefix" {
  description = "Prefix for all resource names"
  type        = string
  default     = "oke"
}

variable "vcn_cidr" {
  description = "VCN CIDR block"
  type        = string
  default     = "10.0.0.0/16"
}

variable "api_subnet_cidr" {
  description = "API/Control Plane subnet CIDR (private)"
  type        = string
  default     = "10.0.0.0/28"
}

variable "node_subnet_cidr" {
  description = "Worker node subnet CIDR (private)"
  type        = string
  default     = "10.0.1.0/24"
}

variable "anywhere_cidr" {
  description = "CIDR for anywhere (internet)"
  type        = string
  default     = "0.0.0.0/0"
}

variable "tcp_protocol_number" {
  description = "Protocol number for TCP"
  type        = string
  default     = "6"
}