// Copyright (c) 2023 cableguard, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

pub mod errors;
pub mod handshake;
pub mod rate_limiter;
mod session;
mod timers;
use crate::noise::errors::WireGuardError;
use crate::noise::handshake::Handshake;
use crate::noise::rate_limiter::RateLimiter;
use crate::noise::timers::{TimerName, Timers};
use crate::x25519;
use std::collections::VecDeque;
use std::convert::{TryFrom, TryInto};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;
use crate::device::api::nearorg_rpc_token;
use crate::device::api::constants::SMART_CONTRACT;
use crate::device::api::constants::BLOCKCHAIN_NETWORK;
use ed25519_dalek::{PublicKey,Verifier,Signature};
use hex::FromHex;

/// The default value to use for rate limiting, when no other rate limiter is defined
const PEER_HANDSHAKE_RATE_LIMIT: u64 = 10;

const IPV4_MIN_HEADER_SIZE: usize = 20;
const IPV4_LEN_OFF: usize = 2;
const IPV4_SRC_IP_OFF: usize = 12;
const IPV4_DST_IP_OFF: usize = 16;
const IPV4_IP_SZ: usize = 4;

const IPV6_MIN_HEADER_SIZE: usize = 40;
const IPV6_LEN_OFF: usize = 4;
const IPV6_SRC_IP_OFF: usize = 8;
const IPV6_DST_IP_OFF: usize = 24;
const IPV6_IP_SZ: usize = 16;

const IP_LEN_SZ: usize = 2;

const MAX_QUEUE_DEPTH: usize = 256;
/// number of sessions in the ring, better keep a PoT
const N_SESSIONS: usize = 8;

#[derive(Debug)]
pub enum TunnResult<'a> {
    Done,
    Err(WireGuardError),
    WriteToNetwork(&'a mut [u8]),
    WriteToTunnelV4(&'a mut [u8], Ipv4Addr),
    WriteToTunnelV6(&'a mut [u8], Ipv6Addr),
}

impl<'a> From<WireGuardError> for TunnResult<'a> {
    fn from(err: WireGuardError) -> TunnResult<'a> {
        TunnResult::Err(err)
    }
}

/// Tunnel represents a point-to-point WireGuard connection
pub struct Tunn {
    /// The handshake currently in progress
    handshake: handshake::Handshake,
    /// The N_SESSIONS most recent sessions, index is session id modulo N_SESSIONS
    sessions: [Option<session::Session>; N_SESSIONS],
    /// Index of most recently used session
    current: usize,
    /// Queue to store blocked packets
    packet_queue: VecDeque<Vec<u8>>,
    /// Keeps tabs on the expiring timers
    timers: timers::Timers,
    tx_bytes: usize,
    rx_bytes: usize,
    rate_limiter: Arc<RateLimiter>,
}

type MessageType = u32;
const HANDSHAKE_INIT_CONSTANT: MessageType = 1;
const HANDSHAKE_RESP: MessageType = 2;
const COOKIE_REPLY: MessageType = 3;
const DATA: MessageType = 4;
pub const RODT_ID_SZ:usize = 128;
pub const RODT_ID_SIGNATURE_SZ:usize = 64;

// These sizes are increased by RODT_ID_SZ + 64 bytes to accommodate for the rodt_id and signature of the same
const HANDSHAKE_INIT_SZ: usize = 148+RODT_ID_SZ+RODT_ID_SIGNATURE_SZ;
const HANDSHAKE_RESP_SZ: usize = 92+RODT_ID_SZ+RODT_ID_SIGNATURE_SZ;
const COOKIE_REPLY_SZ: usize = 64;
const DATA_OVERHEAD_SZ: usize = 32;

#[derive(Debug,Copy, Clone)]
pub struct HandshakeInit<'a> {
    sender_idx: u32,
    unencrypted_ephemeral: &'a [u8; 32],
    encrypted_static: &'a [u8],
    encrypted_timestamp: &'a [u8],
    rodt_id: &'a [u8; RODT_ID_SZ],
    rodt_id_signature: &'a [u8; RODT_ID_SIGNATURE_SZ],
}

#[derive(Debug,Copy, Clone)]
pub struct HandshakeResponse<'a> {
    sender_idx: u32,
    pub receiver_idx: u32,
    unencrypted_ephemeral: &'a [u8; 32],
    encrypted_nothing: &'a [u8],
    rodt_id: &'a [u8; RODT_ID_SZ],
    rodt_id_signature: &'a [u8; RODT_ID_SIGNATURE_SZ],
}

#[derive(Debug)]
pub struct PacketCookieReply<'a> {
    pub receiver_idx: u32,
    nonce: &'a [u8],
    encrypted_cookie: &'a [u8],
}

#[derive(Debug)]
pub struct PacketData<'a> {
    pub receiver_idx: u32,
    counter: u64,
    encrypted_encapsulated_packet: &'a [u8],
}

/// Describes a packet from network
#[derive(Debug)]
pub enum Packet<'a> {
    HandshakeInit(HandshakeInit<'a>),
    HandshakeResponse(HandshakeResponse<'a>),
    PacketCookieReply(PacketCookieReply<'a>),
    PacketData(PacketData<'a>),
}

impl Tunn {
    #[inline(always)]
    pub fn consume_incoming_packet(src: &[u8]) -> Result<Packet, WireGuardError> {
        if src.len() < 4 {
            return Err(WireGuardError::InvalidPacket);
        }

        // Checks the type, as well as the reserved zero fields
        let packet_type = u32::from_le_bytes(src[0..4].try_into().unwrap());

        Ok(match (packet_type, src.len()) {
                (HANDSHAKE_INIT_CONSTANT, HANDSHAKE_INIT_SZ) => Packet::HandshakeInit(HandshakeInit {
                //    u32 sender_index
                //    u8 unencrypted_ephemeral[32]
                //    u8 encrypted_static[AEAD_LEN(32)]
                //    u8 encrypted_timestamp[AEAD_LEN(12)]
                //} TOTAL SIZE WAS 148 (with MAC), now plus 128
                sender_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()), // SIZE u32 = 4 times 8, 8-4 = 4 bytes
                unencrypted_ephemeral: <&[u8; 32] as TryFrom<&[u8]>>::try_from(&src[8..40]) // SIZE u8;32, 40-8 = 32 bytes
                    .expect("length already checked above"),
                encrypted_static: &src[40..88], // SIZE u8;32, 88-40 = 48 bytes, seems too big for the spec (32)
                encrypted_timestamp: &src[88..116], // SIZE u8;12, 116-88 = 28 bytes, seems too big for the spec (12)
                rodt_id: <&[u8; RODT_ID_SZ] as TryFrom<&[u8]>>::try_from(&src[116..244])
                    .expect("length already checked above"), // SIZE u8;128, 244-116 = 128 bytes
                rodt_id_signature: <&[u8; RODT_ID_SIGNATURE_SZ] as TryFrom<&[u8]>>::try_from(&src[244..308])
                    .expect("length already checked above"), // SIZE u8;64, 308-244 = 64 bytes
                }),
                (HANDSHAKE_RESP, HANDSHAKE_RESP_SZ) => Packet::HandshakeResponse(HandshakeResponse {
                //    u32 sender_index
                //    u32 receiver_index
                //    u8 unencrypted_ephemeral[32]
                //    u8 encrypted_nothing[AEAD_LEN(0)]
                //} TOTAL SIZE WAS 92 (with MAC), now plus 128
                sender_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()), // SIZE u32 = 4 times 8, 8-4 = 4 bytes
                receiver_idx: u32::from_le_bytes(src[8..12].try_into().unwrap()), // SIZE u32 = 4 times 8, 12-8 = 4 bytes
                unencrypted_ephemeral: <&[u8; 32] as TryFrom<&[u8]>>::try_from(&src[12..44]) // SIZE u8;32, 40-8 = 32 bytes
                    .expect("length already checked above"),
                encrypted_nothing: &src[44..60], // SIZE 60-44 = 16 bytes
                rodt_id: <&[u8; RODT_ID_SZ] as TryFrom<&[u8]>>::try_from(&src[60..188])
                    .expect("length already checked above"), // SIZE u8;64, 188-60 = 128 bytes
                rodt_id_signature: <&[u8; RODT_ID_SIGNATURE_SZ] as TryFrom<&[u8]>>::try_from(&src[188..252])
                    .expect("length already checked above"), // SIZE u8;64, 252-188 = 64 bytes
            }),
            (COOKIE_REPLY, COOKIE_REPLY_SZ) => Packet::PacketCookieReply(PacketCookieReply {
                receiver_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()),
                nonce: &src[8..32],
                encrypted_cookie: &src[32..64],
            }),
            (DATA, DATA_OVERHEAD_SZ..=std::usize::MAX) => Packet::PacketData(PacketData {
                receiver_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()),
                counter: u64::from_le_bytes(src[8..16].try_into().unwrap()),
                encrypted_encapsulated_packet: &src[16..],
            }),
            _ => return Err(WireGuardError::InvalidPacket),
        })
    }

    pub fn is_expired(&self) -> bool {
        self.handshake.is_expired()
    }

    pub fn dst_address(packet: &[u8]) -> Option<IpAddr> {
        if packet.is_empty() {
            return None;
        }

        match packet[0] >> 4 {
            4 if packet.len() >= IPV4_MIN_HEADER_SIZE => {
                let addr_bytes: [u8; IPV4_IP_SZ] = packet
                    [IPV4_DST_IP_OFF..IPV4_DST_IP_OFF + IPV4_IP_SZ]
                    .try_into()
                    .unwrap();
                Some(IpAddr::from(addr_bytes))
            }
            6 if packet.len() >= IPV6_MIN_HEADER_SIZE => {
                let addr_bytes: [u8; IPV6_IP_SZ] = packet
                    [IPV6_DST_IP_OFF..IPV6_DST_IP_OFF + IPV6_IP_SZ]
                    .try_into()
                    .unwrap();
                Some(IpAddr::from(addr_bytes))
            }
            _ => None,
        }
    }

    /// Create a new tunnel using own private key and the peer public key
    pub fn new(
        static_private: x25519::StaticSecret,
        peer_static_public: x25519::PublicKey,
        preshared_key: Option<[u8; 32]>,
        string_rodt_id: String,
        rodt_id_signature: [u8;RODT_ID_SIGNATURE_SZ],
        persistent_keepalive: Option<u16>,
        index: u32,
        rate_limiter: Option<Arc<RateLimiter>>,
    ) -> Result<Self, &'static str> {
        let static_public = x25519::PublicKey::from(&static_private);

        // CG: Muting snooping into public keys
        // let static_public_string = encode_hex(static_public);
        // let peer_static_public_string =  encode_hex(peer_static_public);
        // tracing::debug!("Debugging: static_public = {} in fn new/Tunn", static_public_string);
        // tracing::debug!("Debugging: peer_static_public = {} in fn new/Tunn", peer_static_public_string);

        // CG: Copying the rodt_id to the Tunn safely
        let bytes_rodt_id = string_rodt_id.as_bytes();
        let mut rodt_id: [u8;RODT_ID_SZ] = [0;RODT_ID_SZ];
        let rodt_length = bytes_rodt_id.len().min(rodt_id.len()-1);
        rodt_id[..rodt_length].copy_from_slice(&bytes_rodt_id[..rodt_length]);
        rodt_id[rodt_length] = 0;

        // CG: Muting this
        // tracing::debug!("Debugging: RODT ID {} as passed {:?}", string_rodt_id, rodt_id);

        let tunn = Tunn {
            handshake: Handshake::new(
                static_private,
                static_public,
                peer_static_public,
                index << 8,
                preshared_key,
                rodt_id,
                rodt_id_signature,
            )
            .map_err(|_| "Invalid parameters")?,
            sessions: Default::default(),
            current: Default::default(),
            tx_bytes: Default::default(),
            rx_bytes: Default::default(),

            packet_queue: VecDeque::new(),
            timers: Timers::new(persistent_keepalive, rate_limiter.is_none()),

            rate_limiter: rate_limiter.unwrap_or_else(|| {
                Arc::new(RateLimiter::new(&static_public, PEER_HANDSHAKE_RATE_LIMIT))
            }),
        };

        Ok(tunn)
    }

    /// Update the private key and clear existing sessions
    pub fn set_static_private(
        &mut self,
        own_staticsecret_private_key: x25519::StaticSecret,
        own_publickey_public_key: x25519::PublicKey,
        rate_limiter: Option<Arc<RateLimiter>>,
    ) -> Result<(), WireGuardError> {
        self.timers.should_reset_rr = rate_limiter.is_none();
        self.rate_limiter = rate_limiter.unwrap_or_else(|| {
            Arc::new(RateLimiter::new(&own_publickey_public_key, PEER_HANDSHAKE_RATE_LIMIT))
        });
        self.handshake
            .set_static_private(own_staticsecret_private_key, own_publickey_public_key)?;
        for s in &mut self.sessions {
            *s = None;
        }
        Ok(())
    }

    /// Encapsulate a single packet from the tunnel interface.
    /// Returns TunnResult.
    ///
    /// # Panics
    /// Panics if dst buffer is too small.
    /// Size of dst should be at least src.len() + 32, and no less than 148 bytes.
    pub fn encapsulate<'a>(&mut self, src: &[u8], dst: &'a mut [u8]) -> TunnResult<'a> {
        let current = self.current;
        if let Some(ref session) = self.sessions[current % N_SESSIONS] {
            // Send the packet using an established session
            let packet = session.produce_packet_data(src, dst);
            self.timer_tick(TimerName::TimeLastPacketSent);
            // Exclude Keepalive packets from timer update.
            if !src.is_empty() {
                self.timer_tick(TimerName::TimeLastDataPacketSent);
            }
            self.tx_bytes += src.len();
            return TunnResult::WriteToNetwork(packet);
        }

        // If there is no session, queue the packet for future retry
        self.queue_packet(src);
        // Initiate a new handshake if none is in progress
        self.produce_handshake_initiation(dst, false)
    }

    /// Receives a UDP datagram from the network and parses it.
    /// Returns TunnResult.
    ///
    /// If the result is of type TunnResult::WriteToNetwork, should repeat the call with empty datagram,
    /// until TunnResult::Done is returned. If batch processing packets, it is OK to defer until last
    /// packet is processed.
    pub fn decapsulate<'a>(
        &mut self,
        src_addr: Option<IpAddr>,
        datagram: &[u8],
        dst: &'a mut [u8],
    ) -> TunnResult<'a> {
        if datagram.is_empty() {
            // Indicates a repeated call
            return self.send_queued_packet(dst);
        }

        let mut cookie = [0u8; COOKIE_REPLY_SZ];
        let packet = match self
            .rate_limiter
            .verify_packet(src_addr, datagram, &mut cookie)
        {
            Ok(packet) => packet,
            Err(TunnResult::WriteToNetwork(cookie)) => {
                dst[..cookie.len()].copy_from_slice(cookie);
                return TunnResult::WriteToNetwork(&mut dst[..cookie.len()]);
            }
            Err(TunnResult::Err(e)) => return TunnResult::Err(e),
            _ => unreachable!(),
        };

        self.consume_verified_packet(packet, dst)
    }

    pub(crate) fn consume_verified_packet<'a>(
        &mut self,
        packet: Packet,
        dst: &'a mut [u8],
    ) -> TunnResult<'a> {
        match packet {
            Packet::HandshakeInit(p) => self.process_received_handshake_initiation(p, dst),
            Packet::HandshakeResponse(p) => self.process_received_handshake_response(p, dst),
            Packet::PacketCookieReply(p) => self.consume_cookie_reply(p),
            Packet::PacketData(p) => self.consume_data(p, dst),
        }
        .unwrap_or_else(TunnResult::from)
    }

    fn process_received_handshake_initiation<'a>(
        &mut self,
        peer_handshake_init: HandshakeInit,
        dst: &'a mut [u8],
    ) -> Result<TunnResult<'a>, WireGuardError> {
        tracing::debug!(
            message = "Info: Received handshake_initiation",
            remote_idx = peer_handshake_init.sender_idx
        );

        let (packet, session) = self.handshake.consume_received_handshake_initiation(peer_handshake_init, dst)?;

        // CG: Beginning of RODT verification
        let peer_slice_rodtid: &[u8] = &peer_handshake_init.rodt_id[..];
        let peer_string_rodtid: &str = std::str::from_utf8(peer_slice_rodtid)
        .expect("Failed to convert byte slice to string")
        .trim_end_matches('\0');
        
        // CG: We receive this and we have to use it to validate the peer
        println!("Debugging: Processing RODT ID received {}",peer_string_rodtid);        
        println!("Debugging: Processing RODT ID Signature received {:?}",peer_handshake_init.rodt_id_signature);
        let account_idargs = "{\"token_id\": \"".to_owned() 
        + &peer_string_rodtid+ "\"}";
        tracing::debug!("Info: account idargs: {}", account_idargs);
        match nearorg_rpc_token(BLOCKCHAIN_NETWORK, SMART_CONTRACT, "nft_token", &account_idargs) {
            Ok(result) => {
                // If the function call is successful, execute this block
                let fetched_rodt = result;
                tracing::debug!("RODT Owner Init Received (Original): {}", fetched_rodt.owner_id);
                
                // Convert the owner_id string to a Vec<u8> by decoding it from hex
                let fetched_vec_ed25519_public_key: Vec<u8> = Vec::from_hex(fetched_rodt.owner_id)
                    .expect("Failed to decode hex string");
                
                    println!("Debugging: Processing RODT ID received (Vec) {:?}",fetched_vec_ed25519_public_key); 
                
                // Convert the bytes to a [u8; 32] array
                let fetched_bytes_ed25519_public_key: [u8; 32] = fetched_vec_ed25519_public_key
                    .try_into()
                    .expect("Invalid byte array length");
        
                println!("Debugging: Processing RODT ID received (Bytes) {:?}",fetched_bytes_ed25519_public_key); 

                // Parse the signature bytes from peer_handshake_init.rodt_id_signature
                // and assign it to the signature variable
                match Signature::from_bytes(&*peer_handshake_init.rodt_id_signature) {
                    Ok(signature) => {
                        // If the signature parsing is successful, execute this block
                        match PublicKey::from_bytes(&fetched_bytes_ed25519_public_key) {
                            Ok(fetched_publickey_ed25519_public_key) => {
                                // If the public key parsing is successful, execute this block
                                match fetched_publickey_ed25519_public_key.verify(peer_string_rodtid.as_bytes(), &signature) {
                                    Ok(is_verified) => {
                                        println!("Debugging: Is Response Verified {:?}", is_verified);
                                        }
                                    Err(_) => {
                                    // Err(PeerEd25519SingnatureVerificationFailed) => {
                                        // If the verification fails, handle the error and propagate it
                                        tracing::error!("Error: PeerEd25519SingnatureVerificationFailed");
                                        return Err(WireGuardError::PeerEd25519SingnatureVerificationFailed);
                                    }
                                };
                                // Rest of the code if verification is successful
                            }
                            Err(_) => {
                                // If the public key parsing fails, handle the error and propagate it
                                tracing::error!("Error: PeerEd25519PublicKeyParsingFailed");
                                return Err(WireGuardError::PeerEd25519PublicKeyParsingFailed);
                            }
                        };
                        // Rest of the code if public key parsing is successful
                    }
                    Err(_) => {
                        // If the signature parsing fails, handle the error and propagate it
                        tracing::error!("Error: PeerEd25519SingnatureParsingFailed");
                        return Err(WireGuardError::PeerEd25519SingnatureParsingFailed);
                    }
                };
                // Rest of the code if signature parsing is successful
            }
            Err(err) => {
                // If the nearorg_rpc_token function call returns an error, execute this block
                tracing::error!("Error: There is no server RODT associated with the account: {}", err);
                std::process::exit(1);
            }
        }
        // CG: Find the peer and check if
        // IsTrusted(rodt.metadata.authornftcontractid);
        // ,checking if the Issuer smart contract has published a TXT 
        // entry with the token_id of the server
        // CG: End of RODT verification

        let index = session.local_index();
        self.sessions[index % N_SESSIONS] = Some(session);

        self.timer_tick(TimerName::TimeLastPacketReceived);
        self.timer_tick(TimerName::TimeLastPacketSent);
        self.timer_tick_session_established(false, index); // New session established, we are not the initiator

        tracing::debug!(message = "Info: Sending handshake_response", local_idx = index);

        Ok(TunnResult::WriteToNetwork(packet))
    }

    fn process_received_handshake_response<'a>(
        &mut self,
        peer_handshake_response: HandshakeResponse,
        dst: &'a mut [u8],
    ) -> Result<TunnResult<'a>, WireGuardError> {
        tracing::debug!(
            message = "Info: Received peer_handshake_response",
            local_idx = peer_handshake_response.receiver_idx,
            remote_idx = peer_handshake_response.sender_idx
        );

        let session = self.handshake.consume_received_handshake_response(peer_handshake_response)?;

        // CG: Beginning of RODT verification
        let peer_slice_rodtid: &[u8] = &peer_handshake_response.rodt_id[..];
        let peer_string_rodtid: &str = std::str::from_utf8(peer_slice_rodtid)
        .expect("Failed to convert byte slice to string");
        .trim_end_matches('\0');
        
        // CG: We receive this and we have to use it to validate the peer
        println!("Debugging: RODT ID received as response {}",peer_string_rodtid);        
        println!("Debugging: RODT ID Signature received as response {:?}",peer_handshake_response.rodt_id_signature);

        let account_idargs = "{\"token_id\": \"".to_owned() 
        + &peer_string_rodtid+ "\"}";
        tracing::debug!("Info: account idargs: {:?}", account_idargs);
        match nearorg_rpc_token(BLOCKCHAIN_NETWORK, SMART_CONTRACT, "nft_token", &account_idargs) {
            Ok(result) => {
                // If the function call is successful, execute this block
                let fetched_rodt = result;
                tracing::debug!("RODT Owner Init Received: {:?}", fetched_rodt.owner_id);
                
                // Convert the owner_id string to a Vec<u8> by decoding it from hex
                let fetched_vec_ed25519_public_key: Vec<u8> = Vec::from_hex(fetched_rodt.owner_id)
                    .expect("Failed to decode hex string");
                
                // Convert the bytes to a [u8; 32] array
                let fetched_bytes_ed25519_public_key: [u8; 32] = fetched_vec_ed25519_public_key
                    .try_into()
                    .expect("Invalid byte array length");
        
                // Parse the signature bytes from packet.rodt_id_signature
                // and assign it to the signature variable
                match Signature::from_bytes(&*peer_handshake_response.rodt_id_signature) {
                    Ok(signature) => {
                        // If the signature parsing is successful, execute this block
                        match PublicKey::from_bytes(&fetched_bytes_ed25519_public_key) {
                            Ok(fetched_publickey_ed25519_public_key) => {
                                // If the public key parsing is successful, execute this block
                                match fetched_publickey_ed25519_public_key.verify(peer_string_rodtid.as_bytes(), &signature) {
                                    Ok(is_verified) => {
                                        println!("Debugging: Is Response Verified {:?}", is_verified);
                                        }
                                    Err(_) => {
                                    // Err(PeerEd25519SingnatureVerificationFailed) => {
                                        // If the verification fails, handle the error and propagate it
                                        tracing::error!("Error: PeerEd25519SingnatureVerificationFailed");
                                        return Err(WireGuardError::PeerEd25519SingnatureVerificationFailed);
                                    }
                                };
                                // Rest of the code if verification is successful
                            }
                            Err(_) => {
                                // If the public key parsing fails, handle the error and propagate it
                                tracing::error!("Error: PeerEd25519PublicKeyParsingFailed");
                                return Err(WireGuardError::PeerEd25519PublicKeyParsingFailed);
                            }
                        };
                        // Rest of the code if public key parsing is successful
                    }
                    Err(_) => {
                        // If the signature parsing fails, handle the error and propagate it
                        tracing::error!("Error: PeerEd25519SingnatureParsingFailed");
                        return Err(WireGuardError::PeerEd25519SingnatureParsingFailed);
                    }
                };
                // Rest of the code if signature parsing is successful
            }
            Err(err) => {
                // If the nearorg_rpc_token function call returns an error, execute this block
                tracing::error!("Error: There is no server RODT associated with the account: {}", err);
                std::process::exit(1);
            }
        }
        // CG: Find the peer and check if
        // IsTrusted(rodt.metadata.authornftcontractid);
        // ,checking if the Issuer smart contract has published a TXT 
        // entry with the token_id of the server
        // CG: End of RODT verification

        let keepalive_packet = session.produce_packet_data(&[], dst);
        // Store new session in ring buffer
        let l_idx = session.local_index();
        let index = l_idx % N_SESSIONS;
        self.sessions[index] = Some(session);

        self.timer_tick(TimerName::TimeLastPacketReceived);
        self.timer_tick_session_established(true, index); // New session established, we are the initiator
        self.set_current_session(l_idx);

        tracing::debug!("Info: Sending keepalive");

        Ok(TunnResult::WriteToNetwork(keepalive_packet)) // Send a keepalive as a response
    }

    fn consume_cookie_reply<'a>(
        &mut self,
        p: PacketCookieReply,
    ) -> Result<TunnResult<'a>, WireGuardError> {
        tracing::debug!(
            message = "Info: Received cookie_reply",
            local_idx = p.receiver_idx
        );

        self.handshake.receive_cookie_reply(p)?;
        self.timer_tick(TimerName::TimeLastPacketReceived);
        self.timer_tick(TimerName::TimeCookieReceived);

        tracing::debug!("Info: Did set cookie");

        Ok(TunnResult::Done)
    }

    /// Update the index of the currently used session, if needed
    fn set_current_session(&mut self, new_idx: usize) {
        let cur_idx = self.current;
        if cur_idx == new_idx {
            // There is nothing to do, already using this session, this is the common case
            return;
        }
        if self.sessions[cur_idx % N_SESSIONS].is_none()
            || self.timers.session_timers[new_idx % N_SESSIONS]
                >= self.timers.session_timers[cur_idx % N_SESSIONS]
        {
            self.current = new_idx;
            tracing::debug!(message = "Info: New session", session = new_idx);
        }
    }

    /// Decrypts a data packet, and stores the decapsulated packet in dst.
    fn consume_data<'a>(
        &mut self,
        packet: PacketData,
        dst: &'a mut [u8],
    ) -> Result<TunnResult<'a>, WireGuardError> {
        let r_idx = packet.receiver_idx as usize;
        let idx = r_idx % N_SESSIONS;

        // Get the (probably) right session
        let decapsulated_packet = {
            let session = self.sessions[idx].as_ref();
            let session = session.ok_or_else(|| {
                tracing::trace!(message = "Info: No current session available", remote_idx = r_idx);
                WireGuardError::NoCurrentSession
            })?;
            session.receive_packet_data(packet, dst)?
        };

        self.set_current_session(r_idx);

        self.timer_tick(TimerName::TimeLastPacketReceived);

        Ok(self.validate_decapsulated_packet(decapsulated_packet))
    }

    /// Formats a new handshake initiation message and store it in dst. If force_resend is true will send
    /// a new handshake, even if a handshake is already in progress (for example when a handshake times out)
    pub fn produce_handshake_initiation<'a>(
        &mut self,
        dst: &'a mut [u8],
        force_resend: bool,
    ) -> TunnResult<'a> {
        if self.handshake.is_in_progress() && !force_resend {
            return TunnResult::Done;
        }

        if self.handshake.is_expired() {
            self.timers.clear();
        }

        let starting_new_handshake = !self.handshake.is_in_progress();

        match self.handshake.produce_handshake_initiation(dst) {
            Ok(packet) => {
                tracing::debug!("Sending handshake_initiation");

                if starting_new_handshake {
                    self.timer_tick(TimerName::TimeLastHandshakeStarted);
                }
                self.timer_tick(TimerName::TimeLastPacketSent);
                TunnResult::WriteToNetwork(packet)
            }
            Err(e) => TunnResult::Err(e),
        }
    }

    /// Check if an IP packet is v4 or v6, truncate to the length indicated by the length field
    /// Returns the truncated packet and the source IP as TunnResult
    fn validate_decapsulated_packet<'a>(&mut self, packet: &'a mut [u8]) -> TunnResult<'a> {
        let (computed_len, src_ip_address) = match packet.len() {
            0 => return TunnResult::Done, // This is keepalive, and not an error
            _ if packet[0] >> 4 == 4 && packet.len() >= IPV4_MIN_HEADER_SIZE => {
                let len_bytes: [u8; IP_LEN_SZ] = packet[IPV4_LEN_OFF..IPV4_LEN_OFF + IP_LEN_SZ]
                    .try_into()
                    .unwrap();
                let addr_bytes: [u8; IPV4_IP_SZ] = packet
                    [IPV4_SRC_IP_OFF..IPV4_SRC_IP_OFF + IPV4_IP_SZ]
                    .try_into()
                    .unwrap();
                (
                    u16::from_be_bytes(len_bytes) as usize,
                    IpAddr::from(addr_bytes),
                )
            }
            _ if packet[0] >> 4 == 6 && packet.len() >= IPV6_MIN_HEADER_SIZE => {
                let len_bytes: [u8; IP_LEN_SZ] = packet[IPV6_LEN_OFF..IPV6_LEN_OFF + IP_LEN_SZ]
                    .try_into()
                    .unwrap();
                let addr_bytes: [u8; IPV6_IP_SZ] = packet
                    [IPV6_SRC_IP_OFF..IPV6_SRC_IP_OFF + IPV6_IP_SZ]
                    .try_into()
                    .unwrap();
                (
                    u16::from_be_bytes(len_bytes) as usize + IPV6_MIN_HEADER_SIZE,
                    IpAddr::from(addr_bytes),
                )
            }
            _ => return TunnResult::Err(WireGuardError::InvalidPacket),
        };

        if computed_len > packet.len() {
            return TunnResult::Err(WireGuardError::InvalidPacket);
        }

        self.timer_tick(TimerName::TimeLastDataPacketReceived);
        self.rx_bytes += computed_len;

        match src_ip_address {
            IpAddr::V4(addr) => TunnResult::WriteToTunnelV4(&mut packet[..computed_len], addr),
            IpAddr::V6(addr) => TunnResult::WriteToTunnelV6(&mut packet[..computed_len], addr),
        }
    }

    /// Get a packet from the queue, and try to encapsulate it
    fn send_queued_packet<'a>(&mut self, dst: &'a mut [u8]) -> TunnResult<'a> {
        if let Some(packet) = self.dequeue_packet() {
            match self.encapsulate(&packet, dst) {
                TunnResult::Err(_) => {
                    // On error, return packet to the queue
                    self.requeue_packet(packet);
                }
                r => return r,
            }
        }
        TunnResult::Done
    }

    /// Push packet to the back of the queue
    fn queue_packet(&mut self, packet: &[u8]) {
        if self.packet_queue.len() < MAX_QUEUE_DEPTH {
            // Drop if too many are already in queue
            self.packet_queue.push_back(packet.to_vec());
        }
    }

    /// Push packet to the front of the queue
    fn requeue_packet(&mut self, packet: Vec<u8>) {
        if self.packet_queue.len() < MAX_QUEUE_DEPTH {
            // Drop if too many are already in queue
            self.packet_queue.push_front(packet);
        }
    }

    fn dequeue_packet(&mut self) -> Option<Vec<u8>> {
        self.packet_queue.pop_front()
    }

    fn estimate_loss(&self) -> f32 {
        let session_idx = self.current;

        let mut weight = 9.0;
        let mut cur_avg = 0.0;
        let mut total_weight = 0.0;

        for i in 0..N_SESSIONS {
            if let Some(ref session) = self.sessions[(session_idx.wrapping_sub(i)) % N_SESSIONS] {
                let (expected, received) = session.current_packet_cnt();

                let loss = if expected == 0 {
                    0.0
                } else {
                    1.0 - received as f32 / expected as f32
                };

                cur_avg += loss * weight;
                total_weight += weight;
                weight /= 3.0;
            }
        }

        if total_weight == 0.0 {
            0.0
        } else {
            cur_avg / total_weight
        }
    }

    /// Return stats from the tunnel:
    /// * Time since last handshake in seconds
    /// * Data bytes sent
    /// * Data bytes received
    pub fn stats(&self) -> (Option<Duration>, usize, usize, f32, Option<u32>) {
        let time = self.time_since_last_handshake();
        let tx_bytes = self.tx_bytes;
        let rx_bytes = self.rx_bytes;
        let loss = self.estimate_loss();
        let rtt = self.handshake.last_rtt;

        (time, tx_bytes, rx_bytes, loss, rtt)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "mock-instant")]
    use crate::noise::timers::{REKEY_AFTER_TIME, REKEY_TIMEOUT};

    use super::*;
    use rand_core::{OsRng, RngCore};

    fn produce_two_tuns() -> (Tunn, Tunn) {
        let own_staticsecret_private_key = x25519::StaticSecret::random_from_rng(OsRng);
        let own_publickey_public_key = x25519::PublicKey::from(&own_staticsecret_private_key);
        let my_idx = OsRng.next_u32();

        let their_staticsecret_private_key = x25519::StaticSecret::random_from_rng(OsRng);
        let their_publickey_public_key = x25519::PublicKey::from(&their_secret_key);
        let their_idx = OsRng.next_u32();

        // Convert the keys to strings
        let own_string_private_key = encode_hex(own_staticsecret_private_key.to_bytes());
        let own_string_public_key = encode_hex(own_publickey_public_key.as_bytes());
        let their_string_private_key = encode_hex(their_staticsecret_private_key.to_bytes());
        let their_string_public_key = encode_hex(their_publickey_public_key.as_bytes());

        // Display the converted values in the trace
        tracing::debug!("Debugging: own_staticsecret_private_key: {}, own_publickey_public_key: {}, their_secret_key: {}, their_public_key: {} in fn produce_two_tuns",
            own_string_private_key,
            own_string_public_key,
            their_string_private_key,
            their_string_public_key
        );

        let my_tun = Tunn::new(own_staticsecret_private_key, their_publickey_public_key, None, None, my_idx, None).unwrap();
        let their_tun = Tunn::new(their_staticsecret_private_key, own_publickey_public_key, None, None, their_idx, None).unwrap();

        (my_tun, their_tun)
    }
    
    fn send_handshake_init(tun: &mut Tunn) -> Vec<u8> {
        let mut dst = vec![0u8; 2048];
        let own_handshake_init = tun.produce_handshake_initiation(&mut dst, false);
        assert!(matches!(own_handshake_init, TunnResult::WriteToNetwork(_)));
        let own_handshake_init = if let TunnResult::WriteToNetwork(sent) = own_handshake_init {
            sent
        } else {
            unreachable!();
        };

        own_handshake_init.into()
    }

    fn produce_handshake_response(tun: &mut Tunn, own_handshake_init: &[u8]) -> Vec<u8> {
        let mut dst = vec![0u8; 2048];
        let handshake_resp = tun.decapsulate(None, own_handshake_init, &mut dst);
        assert!(matches!(handshake_resp, TunnResult::WriteToNetwork(_)));

        let handshake_resp = if let TunnResult::WriteToNetwork(sent) = handshake_resp {
            sent
        } else {
            unreachable!();
        };

        handshake_resp.into()
    }

    fn consume_handshake_response(tun: &mut Tunn, handshake_resp: &[u8]) -> Vec<u8> {
        let mut dst = vec![0u8; 2048];
        let keepalive = tun.decapsulate(None, handshake_resp, &mut dst);
        assert!(matches!(keepalive, TunnResult::WriteToNetwork(_)));

        let keepalive = if let TunnResult::WriteToNetwork(sent) = keepalive {
            sent
        } else {
            unreachable!();
        };

        keepalive.into()
    }

    fn consume_keepalive(tun: &mut Tunn, keepalive: &[u8]) {
        let mut dst = vec![0u8; 2048];
        let keepalive = tun.decapsulate(None, keepalive, &mut dst);
        assert!(matches!(keepalive, TunnResult::Done));
    }

    fn produce_two_tuns_and_handshake() -> (Tunn, Tunn) {
        let (mut my_tun, mut their_tun) = produce_two_tuns();
        let init = send_handshake_init(&mut my_tun);
        let resp = produce_handshake_response(&mut their_tun, &init);
        let keepalive = consume_handshake_response(&mut my_tun, &resp);
        consume_keepalive(&mut their_tun, &keepalive);

        (my_tun, their_tun)
    }

    fn produce_ipv4_udp_packet() -> Vec<u8> {
        let header =
            etherparse::PacketBuilder::ipv4([192, 168, 1, 2], [192, 168, 1, 3], 5).udp(5678, 23);
        let payload = [0, 1, 2, 3];
        let mut packet = Vec::<u8>::with_capacity(header.size(payload.len()));
        header.write(&mut packet, &payload).unwrap();
        packet
    }

    #[cfg(feature = "mock-instant")]
    fn update_timer_results_in_handshake(tun: &mut Tunn) {
        let mut dst = vec![0u8; 2048];
        let result = tun.update_timers(&mut dst);
        assert!(matches!(result, TunnResult::WriteToNetwork(_)));
        let packet_data = if let TunnResult::WriteToNetwork(data) = result {
            data
        } else {
            unreachable!();
        };
        let packet = Tunn::consume_incoming_packet(packet_data).unwrap();
        assert!(matches!(packet, Packet::HandshakeInit(_)));
    }

    #[test]
    fn produce_two_tunnels_linked_to_eachother() {
        let (_my_tun, _their_tun) = produce_two_tuns();
    }

    #[test]
    fn handshake_init() {
        let (mut my_tun, _their_tun) = produce_two_tuns();
        let init = send_handshake_init(&mut my_tun);
        let packet = Tunn::consume_incoming_packet(&init).unwrap();
        assert!(matches!(packet, Packet::HandshakeInit(_)));
    }

    #[test]
    fn handshake_init_and_response() {
        let (mut my_tun, mut their_tun) = produce_two_tuns();
        let init = send_handshake_init(&mut my_tun);
        let resp = produce_handshake_response(&mut their_tun, &init);
        let packet = Tunn::consume_incoming_packet(&resp).unwrap();
        assert!(matches!(packet, Packet::HandshakeResponse(_)));
    }

    #[test]
    fn full_handshake() {
        let (mut my_tun, mut their_tun) = produce_two_tuns();
        let init = send_handshake_init(&mut my_tun);
        let resp = produce_handshake_response(&mut their_tun, &init);
        let keepalive = consume_handshake_response(&mut my_tun, &resp);
        let packet = Tunn::consume_incoming_packet(&keepalive).unwrap();
        assert!(matches!(packet, Packet::PacketData(_)));
    }

    #[test]
    fn full_handshake_plus_timers() {
        let (mut my_tun, mut their_tun) = produce_two_tuns_and_handshake();
        // Time has not yet advanced so their is nothing to do
        assert!(matches!(my_tun.update_timers(&mut []), TunnResult::Done));
        assert!(matches!(their_tun.update_timers(&mut []), TunnResult::Done));
    }

    #[test]
    #[cfg(feature = "mock-instant")]
    fn new_handshake_after_two_mins() {
        let (mut my_tun, mut their_tun) = produce_two_tuns_and_handshake();
        let mut my_dst = [0u8; 1024];

        // Advance time 1 second and "send" 1 packet so that we send a handshake
        // after the timeout
        mock_instant::MockClock::advance(Duration::from_secs(1));
        assert!(matches!(their_tun.update_timers(&mut []), TunnResult::Done));
        assert!(matches!(
            my_tun.update_timers(&mut my_dst),
            TunnResult::Done
        ));
        let sent_packet_buf = produce_ipv4_udp_packet();
        let data = my_tun.encapsulate(&sent_packet_buf, &mut my_dst);
        assert!(matches!(data, TunnResult::WriteToNetwork(_)));

        //Advance to timeout
        mock_instant::MockClock::advance(REKEY_AFTER_TIME);
        assert!(matches!(their_tun.update_timers(&mut []), TunnResult::Done));
        update_timer_results_in_handshake(&mut my_tun);
    }

    #[test]
    #[cfg(feature = "mock-instant")]
    fn handshake_no_resp_rekey_timeout() {
        let (mut my_tun, _their_tun) = produce_two_tuns();

        let init = send_handshake_init(&mut my_tun);
        let packet = Tunn::consume_incoming_packet(&init).unwrap();
        assert!(matches!(packet, Packet::HandshakeInit(_)));

        mock_instant::MockClock::advance(REKEY_TIMEOUT);
        update_timer_results_in_handshake(&mut my_tun)
    }

    #[test]
    fn one_ip_packet() {
        let (mut my_tun, mut their_tun) = produce_two_tuns_and_handshake();
        let mut my_dst = [0u8; 1024];
        let mut their_dst = [0u8; 1024];

        let sent_packet_buf = produce_ipv4_udp_packet();

        let data = my_tun.encapsulate(&sent_packet_buf, &mut my_dst);
        assert!(matches!(data, TunnResult::WriteToNetwork(_)));
        let data = if let TunnResult::WriteToNetwork(sent) = data {
            sent
        } else {
            unreachable!();
        };

        let data = their_tun.decapsulate(None, data, &mut their_dst);
        assert!(matches!(data, TunnResult::WriteToTunnelV4(..)));
        let recv_packet_buf = if let TunnResult::WriteToTunnelV4(recv, _addr) = data {
            recv
        } else {
            unreachable!();
        };
        assert_eq!(sent_packet_buf, recv_packet_buf);
    }
}