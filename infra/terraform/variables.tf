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

variable "valkey_node_type" {
  type        = string
  description = "Memorystore for Valkey node type (e.g. SHARED_CORE_NANO, STANDARD_SMALL)."
  default     = "SHARED_CORE_NANO"
}

variable "valkey_shard_count" {
  type        = number
  description = "Memorystore for Valkey shard count."
  default     = 1
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
