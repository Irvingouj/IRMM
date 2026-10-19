//! Mesh VPN control plane: per-client networks, address allocation, config push.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::Context as _;
use ipnet::{Ipv4Net, Ipv6Net};
use irmm_proto::{MeshConfig, ServerMsg};
use rand::RngCore as _;
use uuid::Uuid;

use crate::db::{self, Tx};

/// Gives every device of a mesh-enabled client its addresses (and the client its ULA prefix).
pub async fn ensure_addrs(tx: &mut Tx, client: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, bool, String, Option<String>)> = sqlx::query_as(
        "SELECT tenant_id, mesh_enabled, mesh_ipv4_range::text, mesh_ipv6_prefix::text FROM clients WHERE id = $1 FOR UPDATE",
    )
    .bind(client)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((tenant, true, range, prefix)) = row else { return Ok(()) };
    let range: Ipv4Net = range.parse().context("mesh range")?;
    let prefix: Ipv6Net = match prefix {
        Some(p) => p.parse().context("mesh prefix")?,
        None => {
            let p = random_ula();
            sqlx::query("UPDATE clients SET mesh_ipv6_prefix = $2::cidr WHERE id = $1")
                .bind(client)
                .bind(p.to_string())
                .execute(&mut **tx)
                .await?;
            p
        }
    };
    let missing: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT d.id FROM devices d JOIN sites s ON s.id = d.site_id
         WHERE s.client_id = $1 AND NOT EXISTS (SELECT 1 FROM mesh_addrs m WHERE m.device_id = d.id)
         ORDER BY d.enrolled_at",
    )
    .bind(client)
    .fetch_all(&mut **tx)
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    let used: Vec<(String, String)> = sqlx::query_as("SELECT host(ipv4), host(ipv6) FROM mesh_addrs WHERE client_id = $1")
        .bind(client)
        .fetch_all(&mut **tx)
        .await?;
    let mut used4: HashSet<Ipv4Addr> = used.iter().filter_map(|u| u.0.parse().ok()).collect();
    let mut used6: HashSet<Ipv6Addr> = used.iter().filter_map(|u| u.1.parse().ok()).collect();
    let net4 = u32::from(range.network());
    let net6 = u128::from(prefix.network());
    for (device,) in missing {
        // Skip the network and broadcast addresses.
        let v4 = (1..u32::MAX.checked_shr(range.prefix_len().into()).unwrap_or(0))
            .map(|i| Ipv4Addr::from(net4 + i))
            .find(|a| !used4.contains(a))
            .context("mesh IPv4 range is full")?;
        let v6 = (1u128..).map(|i| Ipv6Addr::from(net6 + i)).find(|a| !used6.contains(a)).expect("a /48 has room");
        sqlx::query("INSERT INTO mesh_addrs (device_id, tenant_id, client_id, ipv4, ipv6) VALUES ($1, $2, $3, $4::inet, $5::inet)")
            .bind(device)
            .bind(tenant)
            .bind(client)
            .bind(v4.to_string())
            .bind(v6.to_string())
            .execute(&mut **tx)
            .await?;
        used4.insert(v4);
        used6.insert(v6);
    }
    Ok(())
}

fn random_ula() -> Ipv6Net {
    let mut b = [0u8; 16];
    b[0] = 0xfd;
    rand::rng().fill_bytes(&mut b[1..6]);
    Ipv6Net::new(Ipv6Addr::from(b), 48).expect("valid prefix")
}

/// The device's mesh config; `None` when its client has mesh off.
pub async fn config_for(tx: &mut Tx, device: Uuid) -> anyhow::Result<Option<MeshConfig>> {
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT host(m.ipv4), host(m.ipv6), c.mesh_ipv4_range::text, c.mesh_ipv6_prefix::text
         FROM mesh_addrs m JOIN clients c ON c.id = m.client_id
         WHERE m.device_id = $1 AND c.mesh_enabled",
    )
    .bind(device)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((v4, v6, range, prefix)) = row else { return Ok(None) };
    let (range, prefix): (Ipv4Net, Ipv6Net) = (range.parse()?, prefix.parse()?);
    Ok(Some(MeshConfig {
        ipv4: Ipv4Net::new(v4.parse()?, range.prefix_len())?,
        ipv6: Ipv6Net::new(v6.parse()?, prefix.prefix_len())?,
    }))
}

/// Allocates the device's addresses if needed and sends its config to it.
pub async fn on_connect(st: &crate::State, device: Uuid, tenant: Uuid) -> anyhow::Result<()> {
    let mut tx = db::tenant_tx(&st.pool, tenant).await?;
    let (client,): (Uuid,) = sqlx::query_as("SELECT s.client_id FROM devices d JOIN sites s ON s.id = d.site_id WHERE d.id = $1")
        .bind(device)
        .fetch_one(&mut *tx)
        .await?;
    ensure_addrs(&mut tx, client).await?;
    tx.commit().await?;
    send_configs(st, tenant, "d.id = $1", device).await
}

/// Sends the current config to every online device of the client.
pub async fn push_client(st: &crate::State, tenant: Uuid, client: Uuid) -> anyhow::Result<()> {
    send_configs(st, tenant, "s.client_id = $1", client).await
}

/// Reads configs after the caller's commit and sends them under one lock, so an agent
/// always ends up with the newest committed config even when pushes race.
async fn send_configs(st: &crate::State, tenant: Uuid, filter: &str, id: Uuid) -> anyhow::Result<()> {
    let _ordered = st.mesh_push.lock().await;
    let mut tx = db::tenant_tx(&st.pool, tenant).await?;
    let devices: Vec<(Uuid,)> = sqlx::query_as(&format!("SELECT d.id FROM devices d JOIN sites s ON s.id = d.site_id WHERE {filter}"))
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    for (device,) in devices {
        if st.agents.is_online(device) {
            let config = config_for(&mut tx, device).await?;
            st.agents.send(device, ServerMsg::Mesh { config });
        }
    }
    tx.commit().await?;
    Ok(())
}
