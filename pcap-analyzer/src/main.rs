use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use comfy_table::{presets::UTF8_FULL, Table};
use pcap_file::pcap::PcapReader;
use pcap_file::pcapng::{Block, PcapNgReader};
use pcap_file::PcapError;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

// Include prost-generated code from webrtc.proto
mod webrtc_proto {
    include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
}

#[derive(Parser)]
#[command(name = "pcap-analyzer")]
#[command(about = "Analyze WebRTC messages in SCTP pcap files")]
struct Args {
    /// Path to pcap/pcapng file
    pcap_file: PathBuf,

    /// IP address of the dialer (optional)
    #[arg(long)]
    dialer_ip: Option<String>,

    /// Show all messages, not just those with flags
    #[arg(long)]
    all_messages: bool,

    /// Output results to CSV file instead of table
    #[arg(long)]
    csv: bool,

    /// Show payload information (length and first bytes)
    #[arg(long)]
    show_payload: bool,

    /// Filter to only show messages with payloads larger than this size
    #[arg(long)]
    min_payload_size: Option<usize>,

    /// Dump full payload bytes as hex for messages matching criteria
    #[arg(long)]
    dump_payload: bool,

    /// Analyze payload type (handshake vs block announce)
    #[arg(long)]
    analyze_payload: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sender {
    Dialer,
    Listener,
    Unknown,
}

impl std::fmt::Display for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sender::Dialer => write!(f, "Dialer"),
            Sender::Listener => write!(f, "Listener"),
            Sender::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Fin,
    StopSending,
    ResetStream,
    FinAck,
}

impl std::fmt::Display for Flag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Flag::Fin => write!(f, "FIN"),
            Flag::StopSending => write!(f, "STOP_SENDING"),
            Flag::ResetStream => write!(f, "RESET_STREAM"),
            Flag::FinAck => write!(f, "FIN_ACK"),
        }
    }
}

#[derive(Debug, Clone)]
enum SctpEvent {
    /// Outgoing SSN Reset Request (param type 13) - sender wants to reset these outgoing streams
    OutgoingReset { stream_ids: Vec<u16> },
    /// Incoming SSN Reset Request (param type 14) - sender wants to reset these incoming streams
    IncomingReset { stream_ids: Vec<u16> },
    /// Re-configuration Response (param type 16)
    ResetResponse { result: u32, seq_number: u32 },
}

impl SctpEvent {
    fn stream_ids_str(&self) -> String {
        match self {
            SctpEvent::OutgoingReset { stream_ids } | SctpEvent::IncomingReset { stream_ids } => {
                stream_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            SctpEvent::ResetResponse { .. } => "-".to_string(),
        }
    }

    fn description(&self) -> String {
        match self {
            SctpEvent::OutgoingReset { .. } => {
                format!("SCTP Outgoing Reset [{}]", self.stream_ids_str())
            }
            SctpEvent::IncomingReset { .. } => {
                format!("SCTP Incoming Reset [{}]", self.stream_ids_str())
            }
            SctpEvent::ResetResponse { result, seq_number } => {
                let result_str = match result {
                    0 => "Success (Nothing to do)",
                    1 => "Success (Performed)",
                    2 => "Denied",
                    3 => "Error (Wrong SSN)",
                    4 => "Error (Request In Progress)",
                    5 => "Error (Bad Sequence Number)",
                    6 => "In Progress",
                    _ => "Unknown",
                };
                format!("SCTP Reset Response: {} (seq={})", result_str, seq_number)
            }
        }
    }
}

#[derive(Debug)]
struct Message {
    packet_number: u64,
    timestamp: DateTime<Utc>,
    sender: Sender,
    stream_id: u16,
    flag: Option<Flag>,
    protocol: Option<String>,
    payload: Option<Vec<u8>>,
    payload_len: usize,
    sctp_event: Option<SctpEvent>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Determine file format and read packets
    let (messages, total_packets) = if is_pcapng(&args.pcap_file)? {
        read_pcapng(&args.pcap_file, &args)?
    } else {
        read_pcap(&args.pcap_file, &args)?
    };

    // Filter messages if needed
    let mut filtered_messages: Vec<_> = if args.all_messages {
        messages
    } else {
        messages
            .into_iter()
            .filter(|m| {
                m.flag.is_some() || (args.analyze_payload && m.sctp_event.is_some())
            })
            .collect()
    };

    // Apply payload size filter if specified
    if let Some(min_size) = args.min_payload_size {
        filtered_messages = filtered_messages
            .into_iter()
            .filter(|m| m.payload_len >= min_size)
            .collect();
    }

    // Display results
    display_results(&filtered_messages, &args, total_packets)?;

    Ok(())
}

fn is_pcapng(path: &PathBuf) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;

    // pcapng magic: 0x0A0D0D0A
    // pcap magic: 0xA1B2C3D4 or 0xD4C3B2A1 (different endianness)
    Ok(magic == [0x0A, 0x0D, 0x0D, 0x0A])
}

fn read_pcap(path: &PathBuf, args: &Args) -> Result<(Vec<Message>, u64)> {
    let file = File::open(path).context("Failed to open pcap file")?;
    let mut pcap_reader = PcapReader::new(file).context("Failed to create pcap reader")?;

    let mut messages = Vec::new();
    let mut packet_number = 0u64;

    while let Some(packet) = pcap_reader.next_packet() {
        match packet {
            Ok(packet) => {
                packet_number += 1;

                // Convert timestamp to DateTime
                let timestamp = DateTime::from_timestamp(
                    packet.timestamp.as_secs() as i64,
                    packet.timestamp.subsec_nanos(),
                )
                .unwrap_or_else(|| Utc::now());

                // Parse SCTP and extract messages
                if let Some(mut msg_list) = parse_sctp_packet(&packet.data, packet_number, timestamp, &args.dialer_ip) {
                    messages.append(&mut msg_list);
                }
            }
            Err(PcapError::IncompleteBuffer) => break,
            Err(e) => {
                eprintln!("Warning: Error reading packet {}: {}", packet_number + 1, e);
                continue;
            }
        }
    }

    Ok((messages, packet_number))
}

fn read_pcapng(path: &PathBuf, _args: &Args) -> Result<(Vec<Message>, u64)> {
    let file = File::open(path).context("Failed to open pcapng file")?;
    let mut pcapng_reader = PcapNgReader::new(file).context("Failed to create pcapng reader")?;

    let mut messages = Vec::new();
    let mut packet_number = 0u64;

    while let Some(block) = pcapng_reader.next_block() {
        match block {
            Ok(Block::EnhancedPacket(epb)) => {
                packet_number += 1;

                // Convert timestamp to DateTime
                let timestamp = DateTime::from_timestamp(
                    epb.timestamp.as_secs() as i64,
                    epb.timestamp.subsec_nanos(),
                )
                .unwrap_or_else(|| Utc::now());

                // Determine sender from packet direction
                let direction = epb.options.iter().find_map(|opt| {
                    if let pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketOption::Flags(flags) = opt {
                        Some(*flags)
                    } else {
                        None
                    }
                });

                let sender = match direction {
                    Some(flags) if flags & 0x02 != 0 => Sender::Dialer,   // Outbound from browser
                    Some(flags) if flags & 0x01 != 0 => Sender::Listener, // Inbound to browser
                    _ => Sender::Unknown,
                };

                // Parse SCTP and extract messages
                if let Some(mut msg_list) = parse_sctp_packet_with_sender(&epb.data, packet_number, timestamp, sender) {
                    messages.append(&mut msg_list);
                }
            }
            Ok(_) => continue, // Ignore other block types
            Err(PcapError::IncompleteBuffer) => break,
            Err(e) => {
                eprintln!("Warning: Error reading block: {}", e);
                continue;
            }
        }
    }

    Ok((messages, packet_number))
}

fn parse_sctp_packet(data: &[u8], packet_number: u64, timestamp: DateTime<Utc>, _dialer_ip: &Option<String>) -> Option<Vec<Message>> {
    // For pcap files without direction info, use Unknown sender
    // TODO: Could parse IP headers to match against dialer_ip if provided
    let sender = Sender::Unknown;
    parse_sctp_packet_with_sender(data, packet_number, timestamp, sender)
}

fn parse_sctp_packet_with_sender(
    data: &[u8],
    packet_number: u64,
    timestamp: DateTime<Utc>,
    sender: Sender,
) -> Option<Vec<Message>> {
    // Parse Ethernet header (14 bytes)
    if data.len() < 14 {
        return None;
    }

    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    if ethertype != 0x0800 {
        // Not IPv4
        return None;
    }

    let mut offset = 14;

    // Parse IPv4 header
    if offset + 20 > data.len() {
        return None;
    }

    let ip_header_len = ((data[offset] & 0x0F) * 4) as usize;
    let protocol = data[offset + 9];

    if protocol != 132 {
        // Not SCTP
        return None;
    }

    offset += ip_header_len;

    // Parse SCTP common header (12 bytes)
    if offset + 12 > data.len() {
        return None;
    }

    let mut messages = Vec::new();
    offset += 12; // Skip SCTP common header

    // Parse SCTP chunks
    while offset + 4 <= data.len() {
        let chunk_type = data[offset];
        let _chunk_flags = data[offset + 1];
        let chunk_length = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;

        if chunk_length < 4 || offset + chunk_length > data.len() {
            break;
        }

        // Process DATA chunks (type = 0)
        if chunk_type == 0 && chunk_length >= 16 {
            // DATA chunk structure:
            // 0-3: type, flags, length
            // 4-7: TSN
            // 8-9: Stream Identifier
            // 10-11: Stream Sequence Number
            // 12-15: PPID
            // 16+: User Data

            let stream_id = u16::from_be_bytes([data[offset + 8], data[offset + 9]]);
            let ppid = u32::from_be_bytes([
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);

            // Only process WebRTC Binary (PPID = 53)
            if ppid == 53 {
                let user_data = &data[offset + 16..offset + chunk_length];

                // Decode WebRTC messages from user data
                if let Some(mut webrtc_messages) = decode_webrtc_messages(
                    user_data,
                    packet_number,
                    timestamp,
                    sender,
                    stream_id,
                ) {
                    messages.append(&mut webrtc_messages);
                }
            }
        }

        // Process RE-CONFIG chunks (type = 130) per RFC 6525
        // These carry stream reset requests/responses for closing SCTP streams
        if chunk_type == 130 {
            let chunk_end = offset + chunk_length;
            let mut param_offset = offset + 4; // Skip chunk header (type, flags, length)

            while param_offset + 4 <= chunk_end {
                let param_type =
                    u16::from_be_bytes([data[param_offset], data[param_offset + 1]]);
                let param_length =
                    u16::from_be_bytes([data[param_offset + 2], data[param_offset + 3]]) as usize;

                if param_length < 4 || param_offset + param_length > chunk_end {
                    break;
                }

                match param_type {
                    // Outgoing SSN Reset Request Parameter
                    // Format: type(2) + length(2) + req_seq(4) + resp_seq(4) + last_tsn(4) + stream_ids(2*N)
                    13 if param_length >= 16 => {
                        let mut stream_ids = Vec::new();
                        let mut sid_offset = param_offset + 16;
                        while sid_offset + 2 <= param_offset + param_length {
                            let sid = u16::from_be_bytes([data[sid_offset], data[sid_offset + 1]]);
                            stream_ids.push(sid);
                            sid_offset += 2;
                        }

                        let display_stream_id = stream_ids.first().copied().unwrap_or(0);
                        messages.push(Message {
                            packet_number,
                            timestamp,
                            sender,
                            stream_id: display_stream_id,
                            flag: None,
                            protocol: None,
                            payload: None,
                            payload_len: 0,
                            sctp_event: Some(SctpEvent::OutgoingReset {
                                stream_ids,
                            }),
                        });
                    }
                    // Incoming SSN Reset Request Parameter
                    // Format: type(2) + length(2) + req_seq(4) + stream_ids(2*N)
                    14 if param_length >= 8 => {
                        let mut stream_ids = Vec::new();
                        let mut sid_offset = param_offset + 8;
                        while sid_offset + 2 <= param_offset + param_length {
                            let sid = u16::from_be_bytes([data[sid_offset], data[sid_offset + 1]]);
                            stream_ids.push(sid);
                            sid_offset += 2;
                        }

                        let display_stream_id = stream_ids.first().copied().unwrap_or(0);
                        messages.push(Message {
                            packet_number,
                            timestamp,
                            sender,
                            stream_id: display_stream_id,
                            flag: None,
                            protocol: None,
                            payload: None,
                            payload_len: 0,
                            sctp_event: Some(SctpEvent::IncomingReset {
                                stream_ids,
                            }),
                        });
                    }
                    // Re-configuration Response Parameter
                    // Format: type(2) + length(2) + resp_seq(4) + result(4) [+ sender_next_tsn(4) + receiver_next_tsn(4)]
                    16 if param_length >= 12 => {
                        let seq_number = u32::from_be_bytes([
                            data[param_offset + 4],
                            data[param_offset + 5],
                            data[param_offset + 6],
                            data[param_offset + 7],
                        ]);
                        let result = u32::from_be_bytes([
                            data[param_offset + 8],
                            data[param_offset + 9],
                            data[param_offset + 10],
                            data[param_offset + 11],
                        ]);

                        messages.push(Message {
                            packet_number,
                            timestamp,
                            sender,
                            stream_id: 0,
                            flag: None,
                            protocol: None,
                            payload: None,
                            payload_len: 0,
                            sctp_event: Some(SctpEvent::ResetResponse {
                                result,
                                seq_number,
                            }),
                        });
                    }
                    _ => {}
                }

                // Parameters are padded to 4-byte boundary
                param_offset += (param_length + 3) & !3;
            }
        }

        // Move to next chunk (chunks are padded to 4-byte boundary)
        offset += (chunk_length + 3) & !3;
    }

    if messages.is_empty() {
        None
    } else {
        Some(messages)
    }
}

fn decode_webrtc_messages(
    data: &[u8],
    packet_number: u64,
    timestamp: DateTime<Utc>,
    sender: Sender,
    stream_id: u16,
) -> Option<Vec<Message>> {
    let mut messages = Vec::new();
    let mut offset = 0;

    // WebRTC data channel messages are length-prefixed with varints
    while offset < data.len() {
        // Decode varint length
        let (length, varint_len) = match decode_varint(&data[offset..]) {
            Some(v) => v,
            None => break,
        };

        offset += varint_len;

        if offset + length > data.len() {
            break;
        }

        // Decode protobuf message
        let msg_data = &data[offset..offset + length];
        if let Ok(webrtc_msg) = prost::Message::decode(msg_data) {
            let webrtc_msg: webrtc_proto::Message = webrtc_msg;

            let flag = webrtc_msg.flag.and_then(|f| match f {
                0 => Some(Flag::Fin),
                1 => Some(Flag::StopSending),
                2 => Some(Flag::ResetStream),
                3 => Some(Flag::FinAck),
                _ => None,
            });

            // Extract multistream protocol if present
            let protocol = webrtc_msg.message.as_ref().and_then(|msg_bytes| {
                extract_multistream_protocol(msg_bytes)
            });

            // Extract payload bytes and length
            let (payload, payload_len) = match webrtc_msg.message {
                Some(ref bytes) => (Some(bytes.clone()), bytes.len()),
                None => (None, 0),
            };

            messages.push(Message {
                packet_number,
                timestamp,
                sender,
                stream_id,
                flag,
                protocol,
                payload,
                payload_len,
                sctp_event: None,
            });
        }

        offset += length;
    }

    if messages.is_empty() {
        None
    } else {
        Some(messages)
    }
}

fn decode_varint(data: &[u8]) -> Option<(usize, usize)> {
    let mut result = 0usize;
    let mut shift = 0;

    for (i, &byte) in data.iter().enumerate() {
        if i >= 10 {
            // Varints should not exceed 10 bytes
            return None;
        }

        result |= ((byte & 0x7F) as usize) << shift;

        if byte & 0x80 == 0 {
            // Last byte
            return Some((result, i + 1));
        }

        shift += 7;
    }

    None
}

fn analyze_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return "Empty".to_string();
    }

    // Check if it looks like a block announce (typically 200-400 bytes, starts with 32-byte parent hash)
    if payload.len() >= 200 && payload.len() <= 500 {
        // Block announces should start with a 32-byte parent hash
        // After that, byte 32 should be a SCALE compact (usually small value for block number)
        if payload.len() >= 33 {
            let byte_32 = payload[32];
            let mode = byte_32 & 0b11;

            // Mode 0 (single byte) is most common for block numbers in dev chains
            if mode == 0 {
                return format!("Likely BlockAnnounce ({}B)", payload.len());
            }
        }
    }

    // Check if it looks like a handshake (shorter, often starts with specific patterns)
    if payload.len() < 200 {
        return format!("Likely Handshake ({}B)", payload.len());
    }

    format!("Unknown ({}B)", payload.len())
}

fn extract_multistream_protocol(data: &[u8]) -> Option<String> {
    let mut protocols = Vec::new();
    let mut offset = 0;

    // Loop through all length-prefixed strings in the payload
    while offset < data.len() {
        // Decode the length-prefixed string
        let (length, varint_len) = match decode_varint(&data[offset..]) {
            Some(v) => v,
            None => break,
        };

        if offset + varint_len + length > data.len() {
            break;
        }

        let string_data = &data[offset + varint_len..offset + varint_len + length];

        // Try to decode as UTF-8
        if let Ok(s) = std::str::from_utf8(string_data) {
            // Check if it starts with "/" (protocol string) or is "na" (not supported response)
            if s.starts_with('/') || s.trim_end() == "na" {
                protocols.push(s);
            }
        }

        offset += varint_len + length;
    }

    if protocols.is_empty() {
        None
    } else {
        // Join all protocols with spaces
        Some(protocols.join(" "))
    }
}

fn display_results(messages: &[Message], args: &Args, total_packets: u64) -> Result<()> {
    if messages.is_empty() {
        if args.all_messages {
            println!("No messages found in {} packets", total_packets);
        } else {
            println!("No messages with flags found in {} packets", total_packets);
        }
        return Ok(());
    }

    if args.csv {
        write_csv(messages, &args.pcap_file, args)?;
    } else {
        display_table(messages, args.all_messages, total_packets, args);
    }

    Ok(())
}

fn display_table(messages: &[Message], show_all: bool, total_packets: u64, args: &Args) {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);

    // Add payload columns if requested
    let mut headers = vec!["Packet", "Timestamp", "Sender", "StreamID", "Flag", "Protocol"];
    if args.show_payload || args.dump_payload || args.analyze_payload {
        headers.push("PayloadLen");
        if args.analyze_payload {
            headers.push("PayloadType");
        }
        if args.dump_payload {
            headers.push("First128Bytes");
        }
    }
    table.set_header(headers);

    for msg in messages {
        let flag_str = msg.flag
            .map(|f| f.to_string())
            .unwrap_or_else(|| "-".to_string());

        let protocol_str = msg.protocol
            .as_ref()
            .map(|p| p.as_str())
            .unwrap_or("-");

        // For SCTP events, show the target stream IDs instead of the message's stream_id
        let stream_id_str = if let Some(ref event) = msg.sctp_event {
            event.stream_ids_str()
        } else {
            msg.stream_id.to_string()
        };

        let mut row = vec![
            msg.packet_number.to_string(),
            msg.timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            msg.sender.to_string(),
            stream_id_str,
            flag_str,
            protocol_str.to_string(),
        ];

        if args.show_payload || args.dump_payload || args.analyze_payload {
            row.push(msg.payload_len.to_string());

            if args.analyze_payload {
                let analysis = if let Some(ref event) = msg.sctp_event {
                    event.description()
                } else if let Some(ref payload) = msg.payload {
                    analyze_payload(payload)
                } else {
                    "No payload".to_string()
                };
                row.push(analysis);
            }

            if args.dump_payload {
                let hex_str = if let Some(ref payload) = msg.payload {
                    let bytes_to_show = payload.len().min(128);
                    format!("[{}]",
                        payload[..bytes_to_show]
                            .iter()
                            .map(|b| format!("{}", b))
                            .collect::<Vec<_>>()
                            .join(", "))
                } else {
                    "-".to_string()
                };
                row.push(hex_str);
            }
        }

        table.add_row(row);
    }

    println!("{}", table);

    let flag_count = messages.iter().filter(|m| m.flag.is_some()).count();
    if show_all {
        println!(
            "\nSummary: {} total messages ({} with flags) in {} packets",
            messages.len(),
            flag_count,
            total_packets
        );
    } else {
        println!("\nSummary: {} messages with flags found in {} packets", flag_count, total_packets);
    }
}

fn write_csv(messages: &[Message], input_path: &PathBuf, args: &Args) -> Result<()> {
    // Generate output filename from input filename
    let output_path = input_path.with_extension("csv");

    let mut file = File::create(&output_path)
        .context(format!("Failed to create CSV file: {}", output_path.display()))?;

    // Write CSV header
    let mut header = "Packet,Timestamp,Sender,StreamID,Flag,Protocol".to_string();
    if args.show_payload || args.dump_payload || args.analyze_payload {
        header.push_str(",PayloadLen");
        if args.analyze_payload {
            header.push_str(",PayloadType");
        }
        if args.dump_payload {
            header.push_str(",First128Bytes");
        }
    }
    writeln!(file, "{}", header)?;

    // Write each message as a CSV row
    for msg in messages {
        let flag_str = msg.flag
            .map(|f| f.to_string())
            .unwrap_or_else(|| String::new());

        let protocol_str = msg.protocol
            .as_ref()
            .map(|p| escape_csv_field(p))
            .unwrap_or_else(|| String::new());

        // For SCTP events, show the target stream IDs instead of the message's stream_id
        let stream_id_str = if let Some(ref event) = msg.sctp_event {
            escape_csv_field(&event.stream_ids_str())
        } else {
            msg.stream_id.to_string()
        };

        let mut row = format!(
            "{},{},{},{},{},{}",
            msg.packet_number,
            msg.timestamp.format("%Y-%m-%d %H:%M:%S%.3f"),
            msg.sender,
            stream_id_str,
            flag_str,
            protocol_str
        );

        if args.show_payload || args.dump_payload || args.analyze_payload {
            row.push_str(&format!(",{}", msg.payload_len));

            if args.analyze_payload {
                let analysis = if let Some(ref event) = msg.sctp_event {
                    escape_csv_field(&event.description())
                } else if let Some(ref payload) = msg.payload {
                    escape_csv_field(&analyze_payload(payload))
                } else {
                    "No payload".to_string()
                };
                row.push_str(&format!(",{}", analysis));
            }

            if args.dump_payload {
                let hex_str = if let Some(ref payload) = msg.payload {
                    let bytes_to_show = payload.len().min(128);
                    escape_csv_field(&format!("[{}]",
                        payload[..bytes_to_show]
                            .iter()
                            .map(|b| format!("{}", b))
                            .collect::<Vec<_>>()
                            .join(", ")))
                } else {
                    String::new()
                };
                row.push_str(&format!(",{}", hex_str));
            }
        }

        writeln!(file, "{}", row)?;
    }

    println!("CSV output written to: {}", output_path.display());
    Ok(())
}

fn escape_csv_field(field: &str) -> String {
    // If field contains comma, quote, or newline, wrap in quotes and escape quotes
    if field.contains(',') || field.contains('"') || field.contains('\n') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_varint_single_byte() {
        let data = [0x00];
        assert_eq!(decode_varint(&data), Some((0, 1)));

        let data = [0x7F];
        assert_eq!(decode_varint(&data), Some((127, 1)));
    }

    #[test]
    fn test_decode_varint_multi_byte() {
        // 300 = 0b100101100 = 0xAC 0x02
        let data = [0xAC, 0x02];
        assert_eq!(decode_varint(&data), Some((300, 2)));

        // 16384 = 0x80 0x80 0x01
        let data = [0x80, 0x80, 0x01];
        assert_eq!(decode_varint(&data), Some((16384, 3)));
    }

    #[test]
    fn test_decode_varint_incomplete() {
        let data = [0x80]; // Incomplete varint
        assert_eq!(decode_varint(&data), None);
    }

    #[test]
    fn test_decode_varint_too_long() {
        let data = [0x80; 11]; // 11 bytes - too long
        assert_eq!(decode_varint(&data), None);
    }

    #[test]
    fn test_sender_display() {
        assert_eq!(Sender::Dialer.to_string(), "Dialer");
        assert_eq!(Sender::Listener.to_string(), "Listener");
        assert_eq!(Sender::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn test_flag_display() {
        assert_eq!(Flag::Fin.to_string(), "FIN");
        assert_eq!(Flag::StopSending.to_string(), "STOP_SENDING");
        assert_eq!(Flag::ResetStream.to_string(), "RESET_STREAM");
        assert_eq!(Flag::FinAck.to_string(), "FIN_ACK");
    }

    #[test]
    fn test_flag_conversion() {
        // Test flag enum values match proto definition
        assert_eq!(webrtc_proto::message::Flag::Fin as i32, 0);
        assert_eq!(webrtc_proto::message::Flag::StopSending as i32, 1);
        assert_eq!(webrtc_proto::message::Flag::ResetStream as i32, 2);
        assert_eq!(webrtc_proto::message::Flag::FinAck as i32, 3);
    }

    #[test]
    fn test_parse_sctp_header_too_short() {
        let data = [0u8; 8]; // Less than 12 bytes
        let result = parse_sctp_packet_with_sender(&data, 1, Utc::now(), Sender::Unknown);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sctp_header_minimum() {
        let data = [0u8; 12]; // Exactly 12 bytes, no chunks
        let result = parse_sctp_packet_with_sender(&data, 1, Utc::now(), Sender::Unknown);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_multistream_protocol() {
        // Test with "/multistream/1.0.0" (length 19 = 0x13)
        let protocol = b"/multistream/1.0.0";
        let mut data = vec![protocol.len() as u8]; // varint length
        data.extend_from_slice(protocol);

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, Some("/multistream/1.0.0".to_string()));
    }

    #[test]
    fn test_extract_multistream_protocol_yamux() {
        // Test with "/yamux/1.0.0" (length 12 = 0x0C)
        let protocol = b"/yamux/1.0.0";
        let mut data = vec![protocol.len() as u8]; // varint length
        data.extend_from_slice(protocol);

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, Some("/yamux/1.0.0".to_string()));
    }

    #[test]
    fn test_extract_multistream_protocol_no_slash() {
        // Test with non-protocol data
        let data = b"\x05hello";
        let result = extract_multistream_protocol(data);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_multistream_protocol_invalid_utf8() {
        // Test with invalid UTF-8
        let mut data = vec![4u8]; // length 4
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // Invalid UTF-8

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_multistream_protocol_incomplete() {
        // Test with incomplete data (length says 10 but only 5 bytes)
        let data = b"\x0Ahello";
        let result = extract_multistream_protocol(data);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_multistream_protocol_multiple() {
        // Test with multiple protocols: "/multistream/1.0.0\n" and "/ipfs/ping/1.0.0\n"
        let protocol1 = b"/multistream/1.0.0\n";
        let protocol2 = b"/ipfs/ping/1.0.0\n";

        let mut data = vec![protocol1.len() as u8];
        data.extend_from_slice(protocol1);
        data.push(protocol2.len() as u8);
        data.extend_from_slice(protocol2);

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, Some("/multistream/1.0.0\n /ipfs/ping/1.0.0\n".to_string()));
    }

    #[test]
    fn test_extract_multistream_protocol_mixed() {
        // Test with protocol followed by non-protocol data
        let protocol = b"/multistream/1.0.0\n";
        let non_protocol = b"hello";

        let mut data = vec![protocol.len() as u8];
        data.extend_from_slice(protocol);
        data.push(non_protocol.len() as u8);
        data.extend_from_slice(non_protocol);

        let result = extract_multistream_protocol(&data);
        // Should only extract the protocol, not the non-protocol data
        assert_eq!(result, Some("/multistream/1.0.0\n".to_string()));
    }

    #[test]
    fn test_extract_multistream_protocol_na() {
        // Test with "na" response (protocol not supported)
        let na = b"na\n";
        let mut data = vec![na.len() as u8];
        data.extend_from_slice(na);

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, Some("na\n".to_string()));
    }

    #[test]
    fn test_extract_multistream_protocol_request_and_na() {
        // Test with protocol request followed by "na" response
        // Simulates: peer A requests /ipfs/ping/1.0.0, peer B responds na
        let protocol1 = b"/multistream/1.0.0\n";
        let protocol2 = b"/ipfs/ping/1.0.0\n";
        let na = b"na\n";

        // Request (A -> B)
        let mut data = vec![protocol1.len() as u8];
        data.extend_from_slice(protocol1);
        data.push(protocol2.len() as u8);
        data.extend_from_slice(protocol2);

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, Some("/multistream/1.0.0\n /ipfs/ping/1.0.0\n".to_string()));

        // Response (B -> A)
        let mut data = vec![protocol1.len() as u8];
        data.extend_from_slice(protocol1);
        data.push(na.len() as u8);
        data.extend_from_slice(na);

        let result = extract_multistream_protocol(&data);
        assert_eq!(result, Some("/multistream/1.0.0\n na\n".to_string()));
    }
}
