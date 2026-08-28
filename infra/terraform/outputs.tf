# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

output "service_url" {
  value       = google_cloud_run_v2_service.gateway.uri
  description = "Cloud Run service URL."
}

output "artifact_registry_repo" {
  value       = google_artifact_registry_repository.tollgate.name
  description = "Artifact Registry repo for tollgate images."
}

output "cloud_sql_instance" {
  value       = google_sql_database_instance.main.connection_name
  description = "Cloud SQL connection name."
}

output "valkey_endpoint" {
  value       = google_memorystore_instance.cache.endpoints[0].connections[0].psc_auto_connection[0].ip_address
  description = "Memorystore for Valkey private PSC endpoint address."
}

output "runtime_service_account" {
  value       = google_service_account.runtime.email
  description = "Runtime service account used by Cloud Run."
}
