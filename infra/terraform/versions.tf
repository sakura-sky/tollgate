# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

terraform {
  required_version = ">= 1.7"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = ">= 6.24, < 7.0"
    }
    # hashicorp/google-beta was required and configured here, but no resource in
    # this module sets `provider = google-beta`, so it only ever cost a second
    # provider download and a second set of credentials to keep valid. Add it
    # back together with the first resource that actually needs a beta-only
    # field, not before.
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
}
