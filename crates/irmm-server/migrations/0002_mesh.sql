-- Mesh VPN: one network per client, off until an admin enables it.
ALTER TABLE clients
    ADD COLUMN mesh_enabled boolean NOT NULL DEFAULT false,
    ADD COLUMN mesh_ipv4_range cidr NOT NULL DEFAULT '100.88.0.0/16',
    -- Random ULA /48, generated when mesh is first enabled.
    ADD COLUMN mesh_ipv6_prefix cidr;

-- A device's mesh addresses, stable for its lifetime.
CREATE TABLE mesh_addrs (
    device_id uuid PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    client_id uuid NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    ipv4 inet NOT NULL,
    ipv6 inet NOT NULL,
    UNIQUE (client_id, ipv4),
    UNIQUE (client_id, ipv6)
);
ALTER TABLE mesh_addrs ENABLE ROW LEVEL SECURITY;
ALTER TABLE mesh_addrs FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON mesh_addrs USING (irmm_bypass() OR tenant_id = irmm_tenant())
    WITH CHECK (irmm_bypass() OR tenant_id = irmm_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON mesh_addrs TO irmm_app;
