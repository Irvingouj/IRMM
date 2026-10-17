-- Application role. Every request runs `SET LOCAL ROLE irmm_app`, so row-level
-- security applies even though the pool connects as the schema owner.
DO $$
BEGIN
    BEGIN
        CREATE ROLE irmm_app NOLOGIN NOBYPASSRLS;
    EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
    END;
    BEGIN
        EXECUTE format('GRANT irmm_app TO %I', current_user);
    EXCEPTION WHEN others THEN NULL;
    END;
END
$$;

CREATE FUNCTION irmm_tenant() RETURNS uuid LANGUAGE sql STABLE AS
$$ SELECT nullif(current_setting('app.tenant_id', true), '')::uuid $$;

CREATE FUNCTION irmm_bypass() RETURNS boolean LANGUAGE sql STABLE AS
$$ SELECT coalesce(current_setting('app.bypass', true), '') = 'on' $$;

CREATE TABLE tenants (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    email text NOT NULL,
    pw_hash text NOT NULL,
    totp_secret text,
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'technician', 'viewer')),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email ON users (lower(email));

CREATE TABLE clients (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE user_clients (
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id uuid NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, client_id)
);

CREATE TABLE auth_sessions (
    token_hash text PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('pending', 'session')),
    pending_secret text,
    expires_at timestamptz NOT NULL
);

CREATE TABLE sites (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    client_id uuid NOT NULL REFERENCES clients(id),
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE enroll_tokens (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    site_id uuid NOT NULL REFERENCES sites(id),
    token_hash text NOT NULL UNIQUE,
    uses_left int NOT NULL,
    expires_at timestamptz NOT NULL,
    created_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE devices (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    site_id uuid NOT NULL REFERENCES sites(id),
    node_id text NOT NULL UNIQUE,
    hostname text NOT NULL,
    os text NOT NULL,
    os_version text NOT NULL,
    arch text NOT NULL,
    agent_version text NOT NULL,
    inventory jsonb NOT NULL,
    enrolled_at timestamptz NOT NULL DEFAULT now(),
    last_seen timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE scripts (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    name text NOT NULL,
    shell text NOT NULL,
    body text NOT NULL,
    created_by uuid REFERENCES users(id),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    script_id uuid REFERENCES scripts(id) ON DELETE SET NULL,
    shell text NOT NULL,
    body text NOT NULL,
    timeout_secs int NOT NULL,
    created_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE job_runs (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    job_id uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    device_id uuid NOT NULL REFERENCES devices(id),
    status text NOT NULL CHECK (status IN ('pending', 'running', 'succeeded', 'failed')),
    exit_code int,
    stdout text,
    stderr text,
    error text,
    started_at timestamptz,
    finished_at timestamptz
);

CREATE TABLE remote_sessions (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    device_id uuid NOT NULL REFERENCES devices(id),
    user_id uuid NOT NULL REFERENCES users(id),
    kind text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE audit_log (
    id bigserial PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    user_id uuid,
    actor_email text,
    action text NOT NULL,
    target text,
    detail jsonb NOT NULL DEFAULT '{}',
    at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION audit_log_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only';
END
$$;
CREATE TRIGGER audit_log_no_update BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_immutable();
CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION audit_log_immutable();

-- Agent releases are global (published by the operator), not tenant data.
CREATE TABLE releases (
    platform text NOT NULL,
    version text NOT NULL,
    sha256 text NOT NULL,
    signature text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (platform, version)
);

-- Row-level security on every tenant table.
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['users', 'clients', 'user_clients', 'auth_sessions', 'sites', 'enroll_tokens',
                             'devices', 'scripts', 'jobs', 'job_runs', 'remote_sessions', 'audit_log']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I USING (irmm_bypass() OR tenant_id = irmm_tenant())
                        WITH CHECK (irmm_bypass() OR tenant_id = irmm_tenant())', t);
    END LOOP;
END
$$;
ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenants FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenants USING (irmm_bypass() OR id = irmm_tenant())
    WITH CHECK (irmm_bypass() OR id = irmm_tenant());

GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO irmm_app;
REVOKE UPDATE, DELETE, TRUNCATE ON audit_log FROM irmm_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO irmm_app;
