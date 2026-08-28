# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

resource "random_password" "db_password" {
  length      = 32
  special     = true
  min_special = 2
}

resource "google_sql_database_instance" "main" {
  name             = "${var.name_prefix}-pg"
  region           = var.region
  database_version = "POSTGRES_16"

  depends_on = [
    google_service_networking_connection.private_vpc_connection,
    google_project_service.services,
  ]

  settings {
    tier              = var.cloud_sql_tier
    availability_type = "ZONAL"
    disk_type         = "PD_SSD"
    disk_size         = var.cloud_sql_disk_size_gb

    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
      transaction_log_retention_days = 7
    }

    ip_configuration {
      ipv4_enabled                                  = false
      private_network                               = google_compute_network.vpc.id
      enable_private_path_for_google_cloud_services = true
    }

    user_labels = var.labels
  }

  deletion_protection = true
}

resource "google_sql_database" "tollgate" {
  name     = "tollgate"
  instance = google_sql_database_instance.main.name
}

resource "google_sql_user" "app" {
  name     = "tollgate"
  instance = google_sql_database_instance.main.name
  password = random_password.db_password.result
}
