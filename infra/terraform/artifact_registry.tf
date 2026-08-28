# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

resource "google_artifact_registry_repository" "tollgate" {
  location      = var.region
  repository_id = var.name_prefix
  description   = "Tollgate container images."
  format        = "DOCKER"
  labels        = var.labels

  depends_on = [google_project_service.services]
}
