output "vcn_id" {
  description = "VCN OCID"
  value       = oci_core_vcn.oke_vcn.id
}

output "api_subnet_id" {
  description = "API subnet OCID (for OKE cluster)"
  value       = oci_core_subnet.api.id
}

output "node_subnet_id" {
  description = "Node subnet OCID (for OKE node pool)"
  value       = oci_core_subnet.node.id
}

output "bastion_id" {
  description = "Bastion service OCID"
  value       = oci_bastion_bastion.bastion.id
}

output "bastion_name" {
  description = "Bastion service name"
  value       = oci_bastion_bastion.bastion.name
}