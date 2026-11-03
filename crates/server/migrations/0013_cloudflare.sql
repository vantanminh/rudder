-- Optional Cloudflare Zero Trust mode: one tunnel for the local server, Access
-- in front of the panel, and SSH through Access for Infrastructure.
-- Secrets are AES-GCM ciphertext; the API never returns them.

CREATE TABLE cloudflare_settings (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    account_id TEXT NOT NULL DEFAULT '',
    zone_id TEXT NOT NULL DEFAULT '',
    zone_name TEXT NOT NULL DEFAULT '',
    api_token_enc BYTEA,
    tunnel_id TEXT,
    tunnel_name TEXT NOT NULL DEFAULT '',
    tunnel_token_enc BYTEA,
    replicas INTEGER NOT NULL DEFAULT 2 CHECK (replicas BETWEEN 1 AND 5),
    -- Traefik serves routes in tunnel mode once the hostnames are published.
    tunnel_published BOOLEAN NOT NULL DEFAULT FALSE,
    published_hosts TEXT[] NOT NULL DEFAULT '{}',
    team_domain TEXT NOT NULL DEFAULT '',
    access_emails TEXT[] NOT NULL DEFAULT '{}',
    access_app_id TEXT,
    access_aud TEXT,
    access_enforced BOOLEAN NOT NULL DEFAULT FALSE,
    agent_bypass BOOLEAN NOT NULL DEFAULT FALSE,
    agent_bypass_app_ids TEXT[] NOT NULL DEFAULT '{}',
    ssh_address TEXT NOT NULL DEFAULT '',
    ssh_users TEXT[] NOT NULL DEFAULT '{}',
    ssh_target_id TEXT,
    ssh_app_id TEXT,
    ssh_ca_public_key TEXT NOT NULL DEFAULT '',
    ssh_ready BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO cloudflare_settings (id) VALUES (TRUE);
