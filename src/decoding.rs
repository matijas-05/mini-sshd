use std::{
    io::{BufReader, Read},
    net::TcpStream,
};

use anyhow::{anyhow, Context, Result};
use log::{log_enabled, trace, Level};
use num_traits::FromPrimitive;
use openssl::symm::{Crypter, Mode};

use crate::{
    encoding::{encode_string, PACKET_LENGTH_SIZE, STRING_LENGTH_SIZE},
    session::Session,
    types::MessageType,
};

pub struct PayloadReader {
    iter: std::vec::IntoIter<u8>,
}

impl PayloadReader {
    pub fn new(payload: Vec<u8>) -> Self {
        PayloadReader {
            iter: payload.into_iter(),
        }
    }

    // RFC 4251 § 5
    pub fn next_name_list(&mut self) -> Result<Vec<String>> {
        trace!("-- BEGIN NAME-LIST DECODING --");

        let iter = self.iter.by_ref();
        let length_bytes = iter.take(PACKET_LENGTH_SIZE).collect::<Vec<u8>>();

        let length = u8_array_to_u32(length_bytes.as_slice())?;
        trace!("length = {} bytes", length);

        let value_bytes = iter.take(length as usize).collect::<Vec<u8>>();
        let value =
            String::from_utf8(value_bytes).context("Failed to decode name-list to string")?;
        trace!("value = {}", value);

        let name_list = value.split(',').map(String::from).collect();
        trace!("name_list = {:?}", name_list);

        trace!("-- END NAME-LIST DECODING --");
        Ok(name_list)
    }

    // RFC 4251 § 5
    pub fn next_string(&mut self) -> Result<Vec<u8>> {
        trace!("-- BEGIN STRING DECODING --");

        let iter = self.iter.by_ref();
        let length_bytes = iter.take(STRING_LENGTH_SIZE).collect::<Vec<u8>>();
        let length = u8_array_to_u32(&length_bytes)?;
        trace!("length = {}", length);

        let string = iter.take(length as usize).collect();
        trace!("string = {:02x?}", string);

        trace!("-- END STRING DECODING --");
        Ok(string)
    }

    pub fn next_byte(&mut self) -> Option<u8> {
        let byte = self.iter.by_ref().next()?;
        Some(byte)
    }

    pub fn next_n_bytes(&mut self, n: usize) -> Vec<u8> {
        let bytes = self.iter.by_ref().take(n).collect();
        bytes
    }
}

#[derive(Debug)]
pub struct DecodedPacket {
    payload: Vec<u8>,
}

impl DecodedPacket {
    pub fn message_type(&self) -> Result<MessageType> {
        if self.payload.is_empty() {
            return Err(anyhow!("Payload is empty"));
        }

        let message_type = u8_to_MessageType(self.payload[0])?;
        Ok(message_type)
    }

    /// Returns the payload without the message type
    pub fn payload(&self) -> Vec<u8> {
        let without_msg_type = &self.payload[1..];
        without_msg_type.to_vec()
    }

    pub fn payload_with_msg_type(&self) -> &Vec<u8> {
        &self.payload
    }
}

// RFC 4253 § 6
pub fn decode_packet(session: &Session) -> Result<DecodedPacket> {
    trace!(
        "-- BEGIN PACKET DECODING{} --",
        if session.kex().finished {
            " (ENCRYPTED)"
        } else {
            ""
        }
    );

    let decoded_packet = if session.kex().finished {
        decode_packet_encrypted(session)?
    } else {
        decode_packet_unencrypted(session.stream())?
    };

    trace!(
        "-- END PACKET DECODING{} --",
        if session.kex().finished {
            " (ENCRYPTED)"
        } else {
            ""
        }
    );
    Ok(decoded_packet)
}
fn decode_packet_encrypted(session: &Session) -> Result<DecodedPacket> {
    let block_size = session
        .algorithms()
        .as_ref()
        .unwrap()
        .encryption_algorithms_client_to_server
        .details
        .block_size;

    let cipher = session
        .algorithms()
        .as_ref()
        .unwrap()
        .encryption_algorithms_client_to_server
        .details
        .cipher;
    let mut decrypter = Crypter::new(
        cipher,
        Mode::Decrypt,
        session.enc_key_client_server(),
        Some(session.iv_client_server()),
    )?;
    decrypter.pad(false);

    // Read first block
    let mut reader = BufReader::new(session.stream());
    let mut first_block = vec![0u8; block_size];
    reader.read_exact(&mut first_block)?;

    // Decrypt first block to get packet length
    let mut first_block_dec = vec![0u8; block_size];
    decrypter.update(&first_block, &mut first_block_dec)?;

    let packet_length_bytes = &first_block_dec[0..PACKET_LENGTH_SIZE];
    let packet_length = u8_array_to_u32(packet_length_bytes)?;
    trace!("packet_length = {}", packet_length);

    // Read rest of encrypted packet
    let mut rest_enc = vec![0u8; packet_length as usize - (block_size - PACKET_LENGTH_SIZE)];
    reader.read_exact(&mut rest_enc)?;

    // Decrypt rest of encrypted packet
    let mut rest_dec = vec![0u8; rest_enc.len()];
    decrypter.update(&rest_enc, &mut rest_dec)?;

    // Join first block and rest of decrypted packet
    let mut packet_dec = first_block_dec[PACKET_LENGTH_SIZE..].to_vec();
    packet_dec.extend(rest_dec);

    let mac_len = session
        .algorithms()
        .as_ref()
        .unwrap()
        .mac_algorithms_client_to_server
        .details
        .hash
        .size();
    let mut mac = vec![0u8; mac_len];
    reader.read_exact(&mut mac)?;

    let valid = session.crypto().as_ref().unwrap().verify_mac(
        session.sequence_number(),
        session.integrity_key_client_server(),
        // For some reason, this has to be encoded as string
        &encode_string(&packet_dec),
        &mac,
    )?;
    if !valid {
        return Err(anyhow!("MAC verification failed"));
    }

    trace!("packet = {:02x?}", packet_dec);

    let payload = get_payload(packet_dec, packet_length)?;
    Ok(DecodedPacket { payload })
}
fn decode_packet_unencrypted(stream: &TcpStream) -> Result<DecodedPacket> {
    let mut reader = BufReader::new(stream);

    let mut packet_length_bytes = [0u8; PACKET_LENGTH_SIZE];
    reader
        .read_exact(&mut packet_length_bytes)
        .context("Failed reading packet_length")?;
    let packet_length = u8_array_to_u32(&packet_length_bytes)?;
    trace!("packet_length = {} bytes", packet_length);

    let mut packet = vec![0u8; packet_length as usize];
    reader
        .read_exact(&mut packet)
        .context("Failed reading packet")?;

    let payload = get_payload(packet, packet_length)?;
    Ok(DecodedPacket { payload })
}
/// `packet` must not contain the packet_length field
fn get_payload(packet: Vec<u8>, packet_length: u32) -> Result<Vec<u8>> {
    let mut reader = packet.into_iter();
    let reader = reader.by_ref();

    let padding_length = *reader.take(1).collect::<Vec<u8>>().first().unwrap();
    trace!("padding_length = {} bytes", padding_length);

    let n1 = packet_length - (padding_length as u32) - 1;
    let payload = reader.take(n1 as usize).collect::<Vec<u8>>();

    if log_enabled!(Level::Trace) {
        trace!("payload = {:?}", String::from_utf8_lossy(&payload));
    }

    let random_padding = reader.take(padding_length as usize).collect::<Vec<u8>>();
    trace!("random_padding = {:02x?}", random_padding);

    let bytes_left = packet_length - 1 - n1 - padding_length as u32;
    if bytes_left != 0 {
        return Err(anyhow!(
            "Didn't decode entire packet, {} bytes left",
            bytes_left
        ));
    }

    Ok(payload)
}

pub fn u8_array_to_u32(array: &[u8]) -> Result<u32> {
    if array.len() != 4 {
        return Err(anyhow!(
            "Cannot convert u8 array of length {} to u32",
            array.len()
        ));
    }

    Ok(u32::from_be_bytes([array[0], array[1], array[2], array[3]]))
}

pub fn u8_to_bool(value: u8) -> Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(anyhow!("Cannot convert u8 of value {} to bool", value)),
    }
}

#[allow(non_snake_case)]
pub fn u8_to_MessageType(value: u8) -> Result<MessageType> {
    MessageType::from_u8(value).context(format!("Failed to cast {} into MessageType", value))
}

pub fn packet_too_short<T>(var_name: &str) -> Result<T> {
    Err(anyhow!(
        "Packet too short - '{}' could not be read",
        var_name
    ))
}
