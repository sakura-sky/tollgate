# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

# Runtime service account used by the Cloud Run service.
resource "google_service_account" "runtime" {
  account_id   = "${var.name_prefix}-runtime"
  display_name = "Tollgate runtime SA"
}

# Roles needed at runtime:
#  - aiplatform.user      → call Vertex AI / Gemini
#  - cloudsql.client      → connect to Cloud SQL via private IP
#  - logging.logWriter    → write structured logs
#  - monitoring.metricWriter → emit metrics
#  - cloudtrace.agent     → write traces
#
# Secret access is granted per-secret in secrets.tf (least privilege), not as a
# project-wide secretmanager.secretAccessor role.
locals {
  runtime_roles = [
    "roles/aiplatform.user",
    "roles/cloudsql.client",
    "roles/logging.logWriter",
    "roles/monitoring.metricWriter",
    "roles/cloudtrace.agent",
  ]
}

resource "google_project_iam_member" "runtime_roles" {
  for_each = toset(local.runtime_roles)
  project  = var.project_id
  role     = each.value
  member   = "serviceAccount:${google_service_account.runtime.email}"
}
