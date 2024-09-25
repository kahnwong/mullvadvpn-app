//! This module takes care of obtaining ephemeral peers, updating the WireGuard configuration and
//! restarting obfuscation and WG tunnels when necessary.

use super::{config::Config, obfuscation::ObfuscatorHandle, CloseMsg, Error, Tunnel};
#[cfg(target_os = "android")]
use std::sync::Mutex;
use std::{
    net::IpAddr,
    sync::{mpsc as sync_mpsc, Arc},
    time::Duration,
};
#[cfg(target_os = "android")]
use talpid_tunnel::tun_provider::TunProvider;

use ipnetwork::IpNetwork;
use talpid_types::net::wireguard::{PresharedKey, PrivateKey, PublicKey};
use tokio::sync::Mutex as AsyncMutex;

const INITIAL_PSK_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PSK_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(48);
const PSK_EXCHANGE_TIMEOUT_MULTIPLIER: u32 = 2;

#[cfg(windows)]
pub async fn config_ephemeral_peers(
    tunnel: &Arc<AsyncMutex<Option<Box<dyn Tunnel>>>>,
    config: &mut Config,
    retry_attempt: u32,
    obfuscator: Arc<AsyncMutex<Option<ObfuscatorHandle>>>,
    close_obfs_sender: sync_mpsc::Sender<CloseMsg>,
) -> std::result::Result<(), CloseMsg> {
    let iface_name = {
        let tunnel = tunnel.lock().await;
        let tunnel = tunnel.as_ref().unwrap();
        tunnel.get_interface_name()
    };

    log::trace!("Temporarily lowering tunnel MTU before ephemeral peer config");
    try_set_ipv4_mtu(&iface_name, talpid_tunnel::MIN_IPV4_MTU);

    config_ephemeral_peers_inner(tunnel, config, retry_attempt, obfuscator, close_obfs_sender)
        .await?;

    log::trace!("Resetting tunnel MTU");
    try_set_ipv4_mtu(&iface_name, config.mtu);

    Ok(())
}

#[cfg(windows)]
fn try_set_ipv4_mtu(alias: &str, mtu: u16) {
    use talpid_windows::net::*;
    match luid_from_alias(alias) {
        Ok(luid) => {
            if let Err(error) = set_mtu(u32::from(mtu), luid, AddressFamily::Ipv4) {
                log::error!("Failed to set tunnel interface MTU: {error}");
            }
        }
        Err(error) => {
            log::error!("Failed to obtain tunnel interface LUID: {error}")
        }
    }
}

#[cfg(not(windows))]
pub async fn config_ephemeral_peers(
    tunnel: &Arc<AsyncMutex<Option<Box<dyn Tunnel>>>>,
    config: &mut Config,
    retry_attempt: u32,
    obfuscator: Arc<AsyncMutex<Option<ObfuscatorHandle>>>,
    close_obfs_sender: sync_mpsc::Sender<CloseMsg>,
    #[cfg(target_os = "android")] tun_provider: Arc<Mutex<TunProvider>>,
) -> std::result::Result<(), CloseMsg> {
    config_ephemeral_peers_inner(
        tunnel,
        config,
        retry_attempt,
        obfuscator,
        close_obfs_sender,
        #[cfg(target_os = "android")]
        tun_provider,
    )
    .await
}

async fn config_ephemeral_peers_inner(
    tunnel: &Arc<AsyncMutex<Option<Box<dyn Tunnel>>>>,
    config: &mut Config,
    retry_attempt: u32,
    obfuscator: Arc<AsyncMutex<Option<ObfuscatorHandle>>>,
    close_obfs_sender: sync_mpsc::Sender<CloseMsg>,
    #[cfg(target_os = "android")] tun_provider: Arc<Mutex<TunProvider>>,
) -> std::result::Result<(), CloseMsg> {
    let ephemeral_private_key = PrivateKey::new_from_random();
    let close_obfs_sender = close_obfs_sender.clone();

    let exit_should_have_daita = config.daita && !config.is_multihop();
    let exit_psk = request_ephemeral_peer(
        retry_attempt,
        config,
        ephemeral_private_key.public_key(),
        config.quantum_resistant,
        exit_should_have_daita,
    )
    .await?;

    log::debug!("Retrieved ephemeral peer");

    if config.is_multihop() {
        // Set up tunnel to lead to entry
        let mut entry_tun_config = config.clone();
        entry_tun_config
            .entry_peer
            .allowed_ips
            .push(IpNetwork::new(IpAddr::V4(config.ipv4_gateway), 32).unwrap());

        let close_obfs_sender = close_obfs_sender.clone();
        let entry_config = reconfigure_tunnel(
            tunnel,
            entry_tun_config,
            obfuscator.clone(),
            close_obfs_sender,
            #[cfg(target_os = "android")]
            &tun_provider,
        )
        .await?;
        let entry_psk = request_ephemeral_peer(
            retry_attempt,
            &entry_config,
            ephemeral_private_key.public_key(),
            config.quantum_resistant,
            config.daita,
        )
        .await?;
        log::debug!("Successfully exchanged PSK with entry peer");

        config.entry_peer.psk = entry_psk;
    }

    config.exit_peer_mut().psk = exit_psk;
    #[cfg(daita)]
    if config.daita {
        log::trace!("Enabling constant packet size for entry peer");
        config.entry_peer.constant_packet_size = true;
    }

    config.tunnel.private_key = ephemeral_private_key;

    *config = reconfigure_tunnel(
        tunnel,
        config.clone(),
        obfuscator,
        close_obfs_sender,
        #[cfg(target_os = "android")]
        &tun_provider,
    )
    .await?;

    #[cfg(daita)]
    if config.daita {
        // Start local DAITA machines
        let mut tunnel = tunnel.lock().await;
        if let Some(tunnel) = tunnel.as_mut() {
            tunnel
                .start_daita()
                .map_err(Error::TunnelError)
                .map_err(CloseMsg::SetupError)?;
        }
    }

    Ok(())
}

/// Reconfigures the tunnel to use the provided config while potentially modifying the config
/// and restarting the obfuscation provider. Returns the new config used by the new tunnel.
async fn reconfigure_tunnel(
    tunnel: &Arc<AsyncMutex<Option<Box<dyn Tunnel>>>>,
    mut config: Config,
    obfuscator: Arc<AsyncMutex<Option<ObfuscatorHandle>>>,
    close_obfs_sender: sync_mpsc::Sender<CloseMsg>,
    #[cfg(target_os = "android")] tun_provider: &Arc<Mutex<TunProvider>>,
) -> std::result::Result<Config, CloseMsg> {
    let mut obfs_guard = obfuscator.lock().await;
    if let Some(obfuscator_handle) = obfs_guard.take() {
        obfuscator_handle.abort();
        *obfs_guard = super::obfuscation::apply_obfuscation_config(
            &mut config,
            close_obfs_sender,
            #[cfg(target_os = "android")]
            tun_provider.clone(),
        )
        .await
        .map_err(CloseMsg::ObfuscatorFailed)?;
    }

    let mut tunnel = tunnel.lock().await;

    let set_config_future = tunnel
        .as_mut()
        .map(|tunnel| tunnel.set_config(config.clone()));

    if let Some(f) = set_config_future {
        f.await
            .map_err(Error::TunnelError)
            .map_err(CloseMsg::SetupError)?;
    }

    Ok(config)
}

async fn request_ephemeral_peer(
    retry_attempt: u32,
    config: &Config,
    wg_psk_pubkey: PublicKey,
    enable_pq: bool,
    enable_daita: bool,
) -> std::result::Result<Option<PresharedKey>, CloseMsg> {
    log::debug!("Requesting ephemeral peer");

    let timeout = std::cmp::min(
        MAX_PSK_EXCHANGE_TIMEOUT,
        INITIAL_PSK_EXCHANGE_TIMEOUT
            .saturating_mul(PSK_EXCHANGE_TIMEOUT_MULTIPLIER.saturating_pow(retry_attempt)),
    );

    let ephemeral = tokio::time::timeout(
        timeout,
        talpid_tunnel_config_client::request_ephemeral_peer(
            config.ipv4_gateway,
            config.tunnel.private_key.public_key(),
            wg_psk_pubkey,
            enable_pq,
            enable_daita,
        ),
    )
    .await
    .map_err(|_timeout_err| {
        log::warn!("Timeout while negotiating ephemeral peer");
        CloseMsg::EphemeralPeerNegotiationTimeout
    })?
    .map_err(Error::EphemeralPeerNegotiationError)
    .map_err(CloseMsg::SetupError)?;

    Ok(ephemeral.psk)
}