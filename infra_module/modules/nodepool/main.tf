data "oci_identity_availability_domains" "ads" {
  compartment_id = var.compartment_ocid
}

data "oci_core_images" "node_pool_images" {
  compartment_id           = var.compartment_ocid
  operating_system         = "Oracle Linux"
  operating_system_version = "8"
  shape                    = var.node_shape
  sort_by                  = "TIMECREATED"
  sort_order               = "DESC"
}

resource "oci_containerengine_node_pool" "this" {
  cluster_id         = var.cluster_id
  compartment_id     = var.compartment_ocid
  kubernetes_version = var.kubernetes_version
  name               = "${var.name_prefix}-pool"

  node_config_details {
    dynamic "placement_configs" {
      for_each = data.oci_identity_availability_domains.ads.availability_domains
      content {
        availability_domain = placement_configs.value.name
        subnet_id           = var.node_subnet_id
      }
    }
    size = var.node_pool_size
  }

  node_shape = var.node_shape

  node_shape_config {
    memory_in_gbs = var.node_memory_gbs
    ocpus         = var.node_ocpus
  }

  node_source_details {
    image_id    = data.oci_core_images.node_pool_images.images[0].id
    source_type = "image"
  }

  ssh_public_key = var.ssh_public_key
}
