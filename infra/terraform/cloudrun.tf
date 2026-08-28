# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

resource "google_cloud_run_v2_service" "gateway" {
  name     = "${var.name_prefix}-gateway"
  location = var.region
  ingress  = var.allow_public_invoke ? "INGRESS_TRAFFIC_ALL" : "INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER"

  labels = var.labels

  template {
    service_account = google_service_account.runtime.email
    timeout         = "60s"

    scaling {
      min_instance_count = var.cloud_run_min_instances
      max_instance_count = var.cloud_run_max_instances
    }

    max_instance_request_concurrency = var.cloud_run_concurrency

    vpc_access {
      connector = google_vpc_access_connector.connector.id
      egress    = "PRIVATE_RANGES_ONLY"
    }

    containers {
      image = var.image

      ports {
        container_port = 8080
      }

      resources {
        limits = {
          cpu    = "1"
          memory = "512Mi"
        }
        cpu_idle          = true
        startup_cpu_boost = true
      }

      env {
        name  = "TOLLGATE_HTTP__BIND"
        value = "0.0.0.0:8080"
      }

      env {
        name = "TOLLGATE_DATABASE__URL"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.db_url.secret_id
            version = "latest"
          }
        }
      }

      env {
        name = "TOLLGATE_SECURITY__API_KEY_PEPPER"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.api_key_pepper.secret_id
            version = "latest"
          }
        }
      }

      env {
        name = "TOLLGATE_REDIS__URL"
        value = format(
          "redis://%s:6379",
          google_memorystore_instance.cache.endpoints[0].connections[0].psc_auto_connection[0].ip_address,
        )
      }

      env {
        name  = "TOLLGATE_TELEMETRY__SERVICE_NAME"
        value = "tollgate"
      }

      startup_probe {
        http_get {
          path = "/healthz"
        }
        initial_delay_seconds = 1
        period_seconds        = 5
        timeout_seconds       = 2
        failure_threshold     = 6
      }

      liveness_probe {
        http_get {
          path = "/healthz"
        }
        period_seconds  = 30
        timeout_seconds = 2
      }
    }
  }

  depends_on = [
    google_project_iam_member.runtime_roles,
    google_sql_database.tollgate,
    google_sql_user.app,
    google_memorystore_instance.cache,
    google_secret_manager_secret_version.db_url,
    google_secret_manager_secret_version.api_key_pepper,
    google_secret_manager_secret_iam_member.db_url_accessor,
    google_secret_manager_secret_iam_member.api_key_pepper_accessor,
  ]
}

resource "google_cloud_run_v2_service_iam_member" "public" {
  count    = var.allow_public_invoke ? 1 : 0
  name     = google_cloud_run_v2_service.gateway.name
  location = google_cloud_run_v2_service.gateway.location
  role     = "roles/run.invoker"
  member   = "allUsers"
}
