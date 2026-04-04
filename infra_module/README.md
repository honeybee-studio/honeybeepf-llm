# OKE Private Cluster Access via Bastion

Access Oracle Kubernetes Engine (OKE) clusters in private subnets through OCI Bastion service.

## Prerequisites

- OCI CLI installed and configured (`~/.oci/config`)
- SSH key pair (`~/.ssh/id_rsa` and `~/.ssh/id_rsa.pub`)
- `kubectl` installed
- IAM permissions (see [Access Requirements](#access-requirements))

## Per-user Terraform environments

This repo supports one isolated Terraform environment per developer under `infra_module/envs/<your-name>`.

- Use `infra_module/envs/sample/` as a template.
- A real example exists at `infra_module/envs/jun/`.

### Create your environment

1. Copy the template and switch into it:

```bash
cp -R infra_module/envs/sample infra_module/envs/<your-name>
cd infra_module/envs/<your-name>
```

2. Edit `dev.tfvars` to customize non‑secret settings (names, sizes, CIDRs). Secrets go via env vars, not the file.

3. Export the required variables in your shell (zsh):

```bash
# OCI IDs (do not commit these)
export TF_VAR_compartment_ocid="<your-compartment-ocid>"
export TF_VAR_tenancy_ocid="<your-tenancy-ocid>"

# Public key for node access
export TF_VAR_ssh_public_key="$(cat ~/.ssh/id_rsa.pub)"
```

Notes:
- The OCI provider reads credentials from `~/.oci/config`. Make sure `oci` CLI works before running Terraform.
- Keep `dev.tfvars` focused on non‑secret config. Env vars override variable values at runtime.

### Plan and apply

From your env directory (`infra_module/envs/<your-name>`):

```bash
terraform init
terraform plan -var-file=dev.tfvars
terraform apply -var-file=dev.tfvars
```

On success, Terraform will output:
- `cluster_id` — your OKE cluster OCID
- `kubeconfig_command` — a handy command to generate kubeconfig

You can then follow the Bastion tunnel steps below to access the private cluster API.

### Destroy (cleanup)

```bash
terraform destroy -var-file=dev.tfvars
```

### Variable reference

The stack expects these key variables (set via env or `dev.tfvars`):
- `compartment_ocid` (string)
- `tenancy_ocid` (string)
- `ssh_public_key` (string)
- `name_prefix`, `cluster_name`, `kubernetes_version`
- Node pool: `node_pool_size`, `node_shape`, `node_ocpus`, `node_memory_gbs`
- Networking: `vcn_cidr`, `api_subnet_cidr`, `node_subnet_cidr`

## Quick Start

### 1. Create Bastion Session and SSH Tunnel

Run the setup script:

```bash
chmod +x bastion-tunnel.sh

./bastion-tunnel.sh
```

The script will:
1. List available bastions → select one
2. List active OKE clusters → select one
3. Create a port-forwarding session
4. Give you ssh command

**Keep this terminal open** - the tunnel must stay running.

### 2. Configure kubectl

In a new terminal:

```bash

# Generate kubeconfig pointing through the tunnel
oci ce cluster create-kubeconfig \                                                                                                                                                   
--cluster-id <your-cluster-ocid> \
--file ~/.kube/config \
--region <your-region> \
--token-version 2.0.0 \
--kube-endpoint PRIVATE_ENDPOINT

# Update cluster endpoint to use the tunnel
kubectl config set-cluster <your-cluster-name> \
  --server=https://127.0.0.1:6443 \
  --insecure-skip-tls-verify=true

# Verify
kubectl get ns
```