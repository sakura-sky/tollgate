# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

variable "project_id" {
  type        = string
  description = "GCP project ID where Tollgate will be deployed."
}

variable "region" {
  type        = string
  description = "GCP region for Cloud Run, Cloud SQL, Memorystore, and Artifact Registry. Required - no default."
}

variable "name_prefix" {
  type        = string
  description = "Prefix applied to all created resources to allow multiple environments per project."
  default     = "tollgate"
}

variable "image" {
  type        = string
  description = "Fully qualified container image, e.g. {region}-docker.pkg.dev/{project}/tollgate/tollgate:{tag}."
}

variable "cloud_sql_tier" {
  type        = string
  description = "Cloud SQL machine tier."
  default     = "db-f1-micro"
}

variable "cloud_sql_disk_size_gb" {
  type        = number
  description = "Initial Cloud SQL disk size in GB."
  default     = 20
}

variable "provider_env" {
  type = map(string)
  description = <<-EOT
    Extra TOLLGATE_* environment variables for the Cloud Run container, for
    enabling upstream providers.

    Every provider is DISABLED by default, so leaving this empty deploys a
    gateway that answers health, metrics and the console and refuses every
    proxied request as unpriced. That is fail-closed and intended; this variable
    is how you opt in.

    Example, Vertex only (no secret needed, the runtime service account carries
    the credential):

      provider_env = {
        TOLLGATE_PROVIDERS__VERTEX__ENABLED  = "true"
        TOLLGATE_PROVIDERS__VERTEX__PROJECT  = "my-project"
        TOLLGATE_PROVIDERS__VERTEX__LOCATION = "us-central1"
      }

    Do NOT put an API key here. Values land in Terraform state in plaintext.
    For Anthropic or a custom OpenAI-compatible upstream, add the key to Secret
    Manager and reference it in cloudrun.tf the way the database URL is.
  EOT
  default     = {}

  validation {
    # Every secret-shaped name, not just API_KEY. Vertex takes a static OAuth
    # token via ACCESS_TOKEN, which is exactly as sensitive and would otherwise
    # have sailed through this guard into Terraform state.
    condition = !anytrue([
      for k in keys(var.provider_env) :
      can(regex("API_KEY|ACCESS_TOKEN|SECRET|PASSWORD|PEPPER|CREDENTIAL|DATABASE__URL", k))
    ])
    error_message = "Do not pass credentials through provider_env: Terraform state stores them in plaintext. Put the value in Secret Manager and reference it with a secret_key_ref in cloudrun.tf, the way the database URL and the API-key pepper already are."
  }
}

variable "valkey_node_type" {
  type        = string
  description = "Memorystore for Valkey node type (e.g. SHARED_CORE_NANO, STANDARD_SMALL)."
  default     = "SHARED_CORE_NANO"
}

variable "valkey_shard_count" {
  type = number
  description = <<-EOT
    Memorystore for Valkey shard count. Leave at 1.

    Budget counter keys carry no hash tag, and a single request reserves against
    several budgets at once (its key, the provider, the model, the global one)
    in one Lua script. Across shards those keys land in different slots and the
    script is refused with CROSSSLOT, so raising this does not scale the gateway,
    it breaks enforcement for every request that spans more than one budget.

    Giving the keys a common hash tag would lift the restriction, and would also
    rename every existing counter, so it is a deliberate migration rather than a
    variable change.
  EOT
  default     = 1

  validation {
    condition     = var.valkey_shard_count == 1
    error_message = "Budget counter keys are not hash-tagged, so a multi-shard Valkey refuses the reserve script with CROSSSLOT. See the comment above before changing this."
  }
}

variable "valkey_engine_version" {
  type        = string
  description = "Memorystore for Valkey engine version."
  default     = "VALKEY_8_0"
}

variable "cloud_run_min_instances" {
  type        = number
  description = "Cloud Run minimum instances."
  default     = 0
}

variable "cloud_run_max_instances" {
  type        = number
  description = "Cloud Run maximum instances."
  default     = 10
}

variable "cloud_run_concurrency" {
  type        = number
  description = "Maximum concurrent requests per Cloud Run instance."
  default     = 80
}

variable "allow_public_invoke" {
  type        = bool
  description = "Whether to grant allUsers the run.invoker role. Set false for VPC-internal deployments."
  default     = false
}

variable "labels" {
  type        = map(string)
  description = "Labels applied to all created resources."
  default = {
    product   = "tollgate"
    component = "gateway"
  }
}
