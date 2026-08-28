# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

# Runtime secrets live in Secret Manager and are injected into Cloud Run by
# reference (secret_key_ref), never as plaintext env values. That keeps the
# secret material out of the Cloud Run service spec and revision history, so a
# principal with run.viewer but not secretmanager.secretAccessor cannot read it.

# Full Postgres connection string (includes the generated password). Assembled
# from the instance's private IP so the app receives a single DATABASE__URL.
resource "google_secret_manager_secret" "db_url" {
  secret_id = "${var.name_prefix}-db-url"
  labels    = var.labels

  replication {
    auto {}
  }

  depends_on = [google_project_service.services]
}

resource "google_secret_manager_secret_version" "db_url" {
  secret = google_secret_manager_secret.db_url.id
  secret_data = format(
    "postgres://tollgate:%s@%s:5432/tollgate?sslmode=require",
    random_password.db_password.result,
    google_sql_database_instance.main.private_ip_address,
  )
}

# Server-side pepper for API-key HMAC. Must be a fixed secret (>=16 bytes) and
# stable across revisions so already-issued keys keep verifying; a random_password
# in state satisfies both. The same value must be supplied to `admin key issue`.
resource "random_password" "api_key_pepper" {
  length  = 48
  special = false
}

resource "google_secret_manager_secret" "api_key_pepper" {
  secret_id = "${var.name_prefix}-api-key-pepper"
  labels    = var.labels

  replication {
    auto {}
  }

  depends_on = [google_project_service.services]
}

resource "google_secret_manager_secret_version" "api_key_pepper" {
  secret      = google_secret_manager_secret.api_key_pepper.id
  secret_data = random_password.api_key_pepper.result
}

# Grant the runtime SA read access to only these two secrets, rather than a
# project-wide accessor role.
resource "google_secret_manager_secret_iam_member" "db_url_accessor" {
  secret_id = google_secret_manager_secret.db_url.secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.runtime.email}"
}

resource "google_secret_manager_secret_iam_member" "api_key_pepper_accessor" {
  secret_id = google_secret_manager_secret.api_key_pepper.secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.runtime.email}"
}
