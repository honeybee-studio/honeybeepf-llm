# TODO: We can add some objects storage for terraform lock file
data "oci_core_services" "all_services" {
  filter {
    name   = "name"
    values = ["All .* Services In Oracle Services Network"]
    regex  = true
  }
}

resource "oci_core_vcn" "oke_vcn" {
  cidr_block     = var.vcn_cidr
  compartment_id = var.compartment_ocid
  display_name   = "${var.name_prefix}-vcn"
  dns_label      = "${var.name_prefix}vcn"
}

resource "oci_core_internet_gateway" "igw" {
  compartment_id = var.compartment_ocid
  display_name   = "${var.name_prefix}-internet-gateway"
  vcn_id         = oci_core_vcn.oke_vcn.id
}

resource "oci_core_nat_gateway" "nat" {
  compartment_id = var.compartment_ocid
  display_name   = "${var.name_prefix}-nat-gateway"
  vcn_id         = oci_core_vcn.oke_vcn.id
}

resource "oci_core_service_gateway" "sgw" {
  compartment_id = var.compartment_ocid
  display_name   = "${var.name_prefix}-service-gateway"
  vcn_id         = oci_core_vcn.oke_vcn.id
  services {
    service_id = data.oci_core_services.all_services.services[0].id
  }
}

## NOT USED FOR NOW
resource "oci_core_route_table" "public_rt" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.oke_vcn.id
  display_name   = "${var.name_prefix}-public-rt"

  route_rules {
    destination       = var.anywhere_cidr
    destination_type  = "CIDR_BLOCK"
    network_entity_id = oci_core_internet_gateway.igw.id
  }
}

resource "oci_core_route_table" "private_rt" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.oke_vcn.id
  display_name   = "${var.name_prefix}-private-rt"

  route_rules {
    destination       = var.anywhere_cidr
    destination_type  = "CIDR_BLOCK"
    network_entity_id = oci_core_nat_gateway.nat.id
  }

  route_rules {
    destination       = data.oci_core_services.all_services.services[0].cidr_block
    destination_type  = "SERVICE_CIDR_BLOCK"
    network_entity_id = oci_core_service_gateway.sgw.id
  }
}

resource "oci_core_security_list" "api" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.oke_vcn.id
  display_name   = "${var.name_prefix}-api-sl"

  egress_security_rules {
    destination = var.anywhere_cidr
    protocol    = "all"
    description = "Allow all outbound"
  }

  ingress_security_rules {
    protocol = var.tcp_protocol_number
    source   = var.vcn_cidr
    tcp_options {
      min = 6443
      max = 6443
    }
    description = "K8s API from VCN (via Bastion port-forward)"
  }

  ingress_security_rules {
    protocol    = var.tcp_protocol_number
    source      = var.vcn_cidr
    description = "All TCP from VCN internal"
  }
}

resource "oci_core_security_list" "node" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.oke_vcn.id
  display_name   = "${var.name_prefix}-node-sl"

  egress_security_rules {
    destination = var.anywhere_cidr
    protocol    = "all"
    description = "Allow all outbound"
  }

  ingress_security_rules {
    protocol    = "all"
    source      = var.vcn_cidr
    description = "All traffic from VCN"
  }

  ingress_security_rules {
    protocol = var.tcp_protocol_number
    source   = var.vcn_cidr
    tcp_options {
      min = 22
      max = 22
    }
    description = "SSH from VCN (via Bastion)"
  }
}

resource "oci_core_subnet" "api" {
  cidr_block                 = var.api_subnet_cidr
  compartment_id             = var.compartment_ocid
  vcn_id                     = oci_core_vcn.oke_vcn.id
  display_name               = "${var.name_prefix}-api-subnet"
  route_table_id             = oci_core_route_table.private_rt.id
  security_list_ids          = [oci_core_security_list.api.id]
  prohibit_public_ip_on_vnic = true
  dns_label                  = "api"
}

resource "oci_core_subnet" "node" {
  cidr_block                 = var.node_subnet_cidr
  compartment_id             = var.compartment_ocid
  vcn_id                     = oci_core_vcn.oke_vcn.id
  display_name               = "${var.name_prefix}-node-subnet"
  route_table_id             = oci_core_route_table.private_rt.id
  security_list_ids          = [oci_core_security_list.node.id]
  prohibit_public_ip_on_vnic = true
  dns_label                  = "node"
}

resource "oci_bastion_bastion" "bastion" {
  compartment_id               = var.compartment_ocid
  bastion_type                 = "STANDARD"
  target_subnet_id             = oci_core_subnet.node.id
  name                         = "${var.name_prefix}-bastion"
  client_cidr_block_allow_list = [0.0.0.0/0]

  # Max session TTL (1 hours = 3600 seconds)
  max_session_ttl_in_seconds = 3600
}