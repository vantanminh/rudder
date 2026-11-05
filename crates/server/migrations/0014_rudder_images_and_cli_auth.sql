-- Rudder Image store (Cloudflare R2 / S3) and CLI browser/device-code authentication.

ALTER TABLE api_tokens
    ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'api_key'
        CHECK (auth_method IN ('api_key', 'browser', 'device_code'));

CREATE TABLE cli_auth_requests (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind TEXT NOT NULL CHECK (kind IN ('browser', 'device_code')),
    device_code TEXT UNIQUE,
    secret_hash TEXT NOT NULL,
    client_name TEXT NOT NULL DEFAULT 'rudder-cli',
    callback_url TEXT,
    scopes TEXT[] NOT NULL DEFAULT ARRAY['read', 'write']::TEXT[],
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'consumed', 'expired')),
    approved_by UUID REFERENCES users (id) ON DELETE CASCADE,
    issued_token_enc BYTEA,
    token_id UUID REFERENCES api_tokens (id) ON DELETE SET NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX cli_auth_requests_device_code_idx
    ON cli_auth_requests (device_code)
    WHERE device_code IS NOT NULL;
CREATE INDEX cli_auth_requests_expires_idx ON cli_auth_requests (expires_at);

CREATE TABLE image_store_settings (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    destination_id UUID REFERENCES s3_destinations (id) ON DELETE SET NULL,
    path_prefix TEXT NOT NULL DEFAULT 'rudder-images',
    r2_keep_count INTEGER CHECK (r2_keep_count IS NULL OR r2_keep_count >= 1),
    r2_keep_days INTEGER CHECK (r2_keep_days IS NULL OR r2_keep_days >= 1),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO image_store_settings (id) VALUES (TRUE);

ALTER TABLE services
    ADD COLUMN image_auto_deploy BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN image_r2_keep_count INTEGER CHECK (image_r2_keep_count IS NULL OR image_r2_keep_count >= 1),
    ADD COLUMN image_r2_keep_days INTEGER CHECK (image_r2_keep_days IS NULL OR image_r2_keep_days >= 1);

CREATE TABLE rudder_images (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    service_id UUID NOT NULL REFERENCES services (id) ON DELETE CASCADE,
    destination_id UUID REFERENCES s3_destinations (id) ON DELETE SET NULL,
    tag TEXT NOT NULL,
    image_ref TEXT NOT NULL,
    git_sha TEXT,
    object_key TEXT NOT NULL,
    compression TEXT NOT NULL DEFAULT 'zstd' CHECK (compression IN ('none', 'gzip', 'zstd')),
    size_bytes BIGINT NOT NULL DEFAULT 0,
    sha256 TEXT NOT NULL,
    r2_present BOOLEAN NOT NULL DEFAULT TRUE,
    pushed_by UUID REFERENCES users (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (service_id, tag)
);
CREATE INDEX rudder_images_service_idx ON rudder_images (service_id, created_at DESC);
