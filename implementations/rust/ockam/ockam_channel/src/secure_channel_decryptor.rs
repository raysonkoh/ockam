use crate::{
    ChannelKeys, CreateResponderChannelMessage, KeyExchangeCompleted, Role, SecureChannelEncryptor,
    SecureChannelError, SecureChannelKeyExchanger, SecureChannelLocalInfo, SecureChannelVault,
};
use ockam_core::compat::sync::Arc;
use ockam_core::compat::{boxed::Box, string::String, vec::Vec};
use ockam_core::{
    async_trait, route, AllowOnwardAddress, AllowSourceAddresses, IncomingAccessControl,
    LocalSourceOnly, Mailboxes,
};
use ockam_core::{
    Address, Any, Decodable, LocalMessage, Result, Route, Routed, TransportMessage, Worker,
};
use ockam_node::{Context, WorkerBuilder};
use tracing::{debug, info};

struct DecryptorReadyState {
    keys: ChannelKeys,
    encryptor_address: Address,
}

/// Secure Channel Decryptor
pub struct SecureChannelDecryptor<V: SecureChannelVault, K: SecureChannelKeyExchanger> {
    role: Role,
    key_exchanger: Option<K>,
    // Used to talk to the other side of the channel
    remote_address: Address,
    // Used to send decrypted messages to the workers on our node
    internal_address: Address,
    /// Optional address to which message is sent after SecureChannel is created
    key_exchange_completed_callback_route: Option<Address>,
    state: Option<DecryptorReadyState>,
    remote_route: Route,
    custom_payload: Option<Vec<u8>>,
    vault: V,
    key_exchange_name: String,
    init: Option<(Vec<u8>, Route)>,
    allowed_encryptor_sources: Vec<Address>, // AllowAll if empty
}

impl<V: SecureChannelVault, K: SecureChannelKeyExchanger> SecureChannelDecryptor<V, K> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_initiator(
        key_exchanger: K,
        remote_address: Address,
        internal_address: Address,
        // Optional address to which message is sent after SecureChannel is created
        key_exchange_completed_callback_route: Option<Address>,
        remote_route: Route,
        custom_payload: Option<Vec<u8>>,
        vault: V,
        allowed_encryptor_sources: Vec<Address>,
    ) -> Result<Self> {
        let key_exchange_name = key_exchanger.name().await?;
        Ok(Self {
            role: Role::Initiator,
            key_exchanger: Some(key_exchanger),
            remote_address,
            internal_address,
            key_exchange_completed_callback_route,
            remote_route,
            custom_payload,
            vault,
            key_exchange_name,
            state: None,
            init: None,
            allowed_encryptor_sources,
        })
    }

    /// New responder
    #[allow(clippy::too_many_arguments)]
    pub async fn new_responder(
        key_exchanger: K,
        remote_address: Address,
        internal_address: Address,
        // Optional address to which message is sent after SecureChannel is created
        key_exchange_completed_callback_route: Option<Address>,
        init_msg: Vec<u8>,
        init_return_route: Route,
        vault: V,
        allowed_encryptor_sources: Vec<Address>,
    ) -> Result<Self> {
        let key_exchange_name = key_exchanger.name().await?;
        Ok(Self {
            role: Role::Responder,
            remote_address,
            internal_address,
            key_exchanger: Some(key_exchanger),
            key_exchange_completed_callback_route,
            remote_route: route![],
            custom_payload: None,
            vault,
            key_exchange_name,
            state: None,
            init: Some((init_msg, init_return_route)),
            allowed_encryptor_sources,
        })
    }

    /// Restore 12-byte nonce needed for AES GCM from 8 byte that we use for noise
    fn convert_nonce_from_small(b: &[u8]) -> Result<[u8; 12]> {
        let bytes: [u8; 8] = b.try_into().map_err(|_| SecureChannelError::InvalidNonce)?;

        let nonce = u64::from_be_bytes(bytes);

        Ok(SecureChannelEncryptor::<V>::convert_nonce_from_u64(nonce).1)
    }

    async fn send_key_exchange_payload(
        &mut self,
        ctx: &mut <Self as Worker>::Context,
        payload: Vec<u8>,
        is_first_initiator_msg: bool,
    ) -> Result<()> {
        if is_first_initiator_msg {
            // First message from initiator goes to the channel listener
            ctx.send_from_address(
                self.remote_route.clone(),
                CreateResponderChannelMessage::new(payload, self.custom_payload.take()),
                self.remote_address.clone(),
            )
            .await
        } else {
            // Other messages go to the channel worker itself
            ctx.send_from_address(
                self.remote_route.clone(),
                payload,
                self.remote_address.clone(),
            )
            .await
        }
    }

    async fn handle_decrypt(
        &mut self,
        ctx: &mut <Self as Worker>::Context,
        msg: Routed<<Self as Worker>::Message>,
    ) -> Result<()> {
        debug!("SecureChannel received Decrypt");

        let state = self
            .state
            .as_mut()
            .ok_or(SecureChannelError::InvalidInternalState)?;

        let transport_message = msg.into_transport_message();
        let payload = transport_message.payload;
        let payload = Vec::<u8>::decode(&payload)?;

        let payload = {
            if payload.len() < 8 {
                return Err(SecureChannelError::InvalidNonce.into());
            }

            let nonce = Self::convert_nonce_from_small(&payload.as_slice()[..8])?;

            self.vault
                .aead_aes_gcm_decrypt(&state.keys.key, &payload[8..], &nonce, &[])
                .await?
        };

        let mut transport_message = TransportMessage::decode(&payload)?;

        transport_message
            .return_route
            .modify()
            .prepend(state.encryptor_address.clone());

        let local_info = SecureChannelLocalInfo::new(self.key_exchange_name.clone());

        let local_msg = LocalMessage::new(transport_message, vec![local_info.to_local_info()?]);

        ctx.forward_from_address(local_msg, self.internal_address.clone())
            .await
    }

    async fn handle_key_exchange_msg(
        &mut self,
        ctx: &mut <Self as Worker>::Context,
        msg: Routed<<Self as Worker>::Message>,
    ) -> Result<()> {
        let reply = msg.return_route();
        let payload = Vec::<u8>::decode(&msg.into_transport_message().payload)?;

        self.handle_key_exchange(ctx, reply, &payload).await
    }

    async fn handle_key_exchange(
        &mut self,
        ctx: &mut <Self as Worker>::Context,
        reply: Route,
        payload: &[u8],
    ) -> Result<()> {
        // Received key exchange message from remote channel, need to forward it to local key exchange
        debug!("SecureChannel received KeyExchangeRemote");

        let key_exchanger = self
            .key_exchanger
            .as_mut()
            .ok_or(SecureChannelError::InvalidInternalState)?;

        // Update route to a remote
        self.remote_route = reply;

        let _ = key_exchanger.handle_response(payload).await?;

        if !key_exchanger.is_complete().await? {
            let payload = key_exchanger.generate_request(&[]).await?;
            let is_now_complete = key_exchanger.is_complete().await?;
            self.send_key_exchange_payload(ctx, payload, false).await?;

            if !is_now_complete {
                return Ok(());
            }
        }

        let key_exchanger = self
            .key_exchanger
            .take()
            .ok_or(SecureChannelError::InvalidInternalState)?;

        let keys = key_exchanger.finalize().await?;

        let role_str = match self.role {
            Role::Initiator => "initiator",
            Role::Responder => "responder",
        };
        let next_hop = self.remote_route.next()?.clone();
        let address_local =
            Address::random_tagged(&format!("SecureChannel.{}.encryptor", role_str));
        let encryptor = SecureChannelEncryptor::new(
            ChannelKeys {
                key: keys.encrypt_key().clone(),
                nonce: 0,
            },
            self.remote_route.clone(),
            self.vault.async_try_clone().await?,
        );
        let incoming_access_control: Arc<dyn IncomingAccessControl> =
            if !self.allowed_encryptor_sources.is_empty() {
                Arc::new(AllowSourceAddresses(self.allowed_encryptor_sources.clone()))
            } else {
                Arc::new(LocalSourceOnly)
            };

        WorkerBuilder::with_mailboxes(
            Mailboxes::main(
                address_local.clone(),
                incoming_access_control,
                Arc::new(AllowOnwardAddress(next_hop)),
            ),
            encryptor,
        )
        .start(ctx)
        .await?;

        info!(
            "Started SecureChannel {} at local: {}, remote: {}",
            self.role.role_str(),
            &address_local,
            &ctx.address()
        );

        // Notify interested worker about finished key exchange
        if let Some(r) = self.key_exchange_completed_callback_route.take() {
            ctx.send_from_address(
                r,
                KeyExchangeCompleted::new(address_local.clone(), *keys.h()),
                self.internal_address.clone(),
            )
            .await?;
        }

        self.state = Some(DecryptorReadyState {
            keys: ChannelKeys {
                key: keys.decrypt_key().clone(),
                nonce: 0,
            },
            encryptor_address: address_local,
        });

        Ok(())
    }
}

#[async_trait]
impl<V: SecureChannelVault, K: SecureChannelKeyExchanger> Worker for SecureChannelDecryptor<V, K> {
    type Message = Any;
    type Context = Context;

    async fn initialize(&mut self, ctx: &mut Self::Context) -> Result<()> {
        match &self.role {
            Role::Initiator => {
                if let Some(key_exchanger) = &mut self.key_exchanger {
                    let payload = key_exchanger.generate_request(&[]).await?;

                    self.send_key_exchange_payload(ctx, payload, true).await?;
                } else {
                    return Err(SecureChannelError::InvalidInternalState.into());
                }
            }
            Role::Responder => {
                if let Some((init_payload, init_return_route)) = self.init.take() {
                    self.handle_key_exchange(ctx, init_return_route, &init_payload)
                        .await?;
                } else {
                    return Err(SecureChannelError::InvalidInternalState.into());
                }
            }
        }

        Ok(())
    }

    async fn handle_message(
        &mut self,
        ctx: &mut Self::Context,
        msg: Routed<Self::Message>,
    ) -> Result<()> {
        let msg_addr = msg.msg_addr();

        if msg_addr == self.remote_address {
            if self.state.is_some() {
                self.handle_decrypt(ctx, msg).await?;
            } else if self.key_exchanger.is_some() {
                self.handle_key_exchange_msg(ctx, msg).await?;
            } else {
                return Err(SecureChannelError::InvalidInternalState.into());
            }
        }

        Ok(())
    }
}
