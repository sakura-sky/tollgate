# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

# Memorystore for Valkey. Valkey is the BSD-licensed Redis fork; it is
# wire-compatible, so the gateway's `redis` client connects unchanged, and the
# managed Valkey tier is cheaper than Memorystore for Redis on GCP.
#
# Unlike the legacy Redis instance (service-networking VPC peering), Memorystore
# for Valkey connects over Private Service Connect. That needs a service
# connection policy authorizing the memorystore service class to auto-create PSC
# endpoints in our subnet. Cloud SQL still uses the peering range in main.tf.

resource "google_network_connectivity_service_connection_policy" "valkey" {
  name          = "${var.name_prefix}-valkey-scp"
  location      = var.region
  service_class = "gcp-memorystore"
  description   = "PSC policy for Tollgate Memorystore for Valkey."
  network       = google_compute_network.vpc.id

  psc_config {
    subnetworks = [google_compute_subnetwork.subnet.id]
  }

  depends_on = [google_project_service.services]
}

resource "google_memorystore_instance" "cache" {
  instance_id = "${var.name_prefix}-valkey"
  location    = var.region

  shard_count    = var.valkey_shard_count
  node_type      = var.valkey_node_type
  engine_version = var.valkey_engine_version

  # Auto-create the PSC endpoints in our VPC via the policy above.
  desired_psc_auto_connections {
    network    = google_compute_network.vpc.id
    project_id = var.project_id
  }

  # Append-only persistence so budget counters survive a node restart. The
  # ledger in Postgres is still the system of record, and the gateway rebuilds
  # any counter it finds missing, but AOF avoids paying for that rebuild on
  # every restart.
  persistence_config {
    mode = "AOF"
    aof_config {
      append_fsync = "EVERY_SEC"
    }
  }

  # Budget counters carry a 40-day expiry, which makes every one of them an
  # eviction candidate under any volatile-* policy. Evicting one is not a cache
  # miss: it is a budget losing its record of the period. The gateway now
  # detects that and rebuilds the counter from the ledger, so the failure mode
  # is a Postgres query per affected budget rather than silent overspend, but
  # under memory pressure that can mean a query per budget per request.
  #
  # noeviction turns that into a write error the operator can see and alert on,
  # instead of a quiet tax on the hot path. Do not share this instance with a
  # workload that wants LRU behaviour.
  engine_configs = {
    "maxmemory-policy" = "noeviction"
  }

  # Unlike Cloud SQL (deletion_protection = true), the cache carries no durable
  # state: it is fully reconstructable from the Postgres ledger on the next boot,
  # so deletion protection is intentionally off to keep teardown simple.
  deletion_protection_enabled = false
  labels                      = var.labels

  depends_on = [
    google_network_connectivity_service_connection_policy.valkey,
    google_project_service.services,
  ]
}
