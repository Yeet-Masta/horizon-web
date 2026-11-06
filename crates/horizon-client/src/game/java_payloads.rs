//! Bounded transport for Java mod payload channels.
//!
//! A native Java mod declares payload channels while its entrypoints
//! bootstrap ([`horizon_native_java::PayloadRegistration`]); the host sends the
//! declaration list to a joining client on [`REGISTER_CHANNEL`], and both sides
//! exchange frames on [`CHANNEL`]. A frame is only accepted when its id is
//! declared, its direction matches, and a codec the transport models validates
//! its bytes. Transport stays separate from the packet loops so the hub side
//! (`game/lan_actions.rs`) and the remote client (`game/remote.rs`) drive the
//! same state machine, and so every bound is testable without a running game.
//!
//! `horizon-mods` carries the payload model (channel declarations, the
//! length-prefixed frame, the codecs) on the branch this transport was written
//! against; main has no copy of it, so it is reproduced at the top of this
//! module, unchanged on the wire. Moving it back to `horizon_mods::payloads`
//! is a mechanical import swap: nothing below the model touches it.

// The bootstrap half below (a jar's declaration list, its handler host and the
// queues its sends go through) is called by the Java mod loader once it has run
// a jar and kept its host (`game/java_mods.rs::run_one`); until that call
// lands, those entry points and their helpers are unused here.
#![allow(dead_code)]

use super::{Game, lan::LanHost};
use horizon_native_java::{
    NativeJar, PayloadRegistration, StdHost, Value, dispatch_payload_frame_with,
};
use horizon_protocol::packets::{configuration::CustomPayload, play::Clientbound as C};
use std::collections::BTreeMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// The payload model (ported from `horizon-mods`: `payloads.rs`).
// ---------------------------------------------------------------------------

/// A payload id is a resource location; a longer one is rejected.
pub(super) const MAX_PAYLOAD_ID: usize = 256;
/// The payload bytes one frame may carry; a larger packet is refused, not
/// buffered.
pub(super) const MAX_PAYLOAD_BYTES: usize = 1 << 20;
/// Declarations one registry holds.
const MAX_PAYLOAD_DECLARATIONS: usize = 4_096;
/// The largest `length` field: the fixed header, a maximal id and payload.
const MAX_FRAME_REST: usize = 3 + MAX_PAYLOAD_ID + MAX_PAYLOAD_BYTES;
/// The most bytes a marker spells out.
pub(super) const MAX_MARKER_BYTES: usize = 256;

/// The channels a `PayloadRegistrar` call names. A frame travels on one
/// concrete channel, so the play directions share
/// [`PayloadDirection::PlayBidirectional`] as a declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum PayloadDirection {
    PlayToClient,
    PlayToServer,
    ConfigurationToClient,
    ConfigurationToServer,
    PlayBidirectional,
}

impl PayloadDirection {
    /// The name a native `PayloadRegistrar` call records (`playToServer`).
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::PlayToClient => "playToClient",
            Self::PlayToServer => "playToServer",
            Self::ConfigurationToClient => "configurationToClient",
            Self::ConfigurationToServer => "configurationToServer",
            Self::PlayBidirectional => "playBidirectional",
        }
    }

    /// Parse a native call name or the snake_case spelling the compatibility
    /// report prints.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "playToClient" | "play_to_client" => Ok(Self::PlayToClient),
            "playToServer" | "play_to_server" => Ok(Self::PlayToServer),
            "configurationToClient" | "configuration_to_client" => Ok(Self::ConfigurationToClient),
            "configurationToServer" | "configuration_to_server" => Ok(Self::ConfigurationToServer),
            "playBidirectional" | "play_bidirectional" => Ok(Self::PlayBidirectional),
            "configurationBidirectional" | "configuration_bidirectional" => Err(
                "payload direction configurationBidirectional has no bounded frame channel"
                    .to_owned(),
            ),
            other => Err(format!("unknown payload direction {other:?}")),
        }
    }

    /// The direction byte a frame carries. The values are part of the wire
    /// format; explicit values mean reordering the enum cannot move a channel.
    pub(super) const fn wire(self) -> u8 {
        match self {
            Self::PlayToClient => 0,
            Self::PlayToServer => 1,
            Self::ConfigurationToClient => 2,
            Self::ConfigurationToServer => 3,
            Self::PlayBidirectional => 4,
        }
    }

    pub(super) fn from_wire(byte: u8) -> Result<Self, String> {
        match byte {
            0 => Ok(Self::PlayToClient),
            1 => Ok(Self::PlayToServer),
            2 => Ok(Self::ConfigurationToClient),
            3 => Ok(Self::ConfigurationToServer),
            4 => Ok(Self::PlayBidirectional),
            other => Err(format!("unknown payload frame direction {other}")),
        }
    }
}

/// One payload channel a mod declares: its namespaced id, direction and the
/// optional stream codec id the registration carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PayloadDeclaration {
    pub(super) id: String,
    pub(super) direction: PayloadDirection,
    pub(super) codec: Option<String>,
}

impl PayloadDeclaration {
    /// The id, and its codec when present, are resource locations.
    fn validate(&self) -> Result<(), String> {
        check_id(&self.id)?;
        if let Some(codec) = &self.codec {
            check_id(codec)?;
        }
        Ok(())
    }
}

/// The payload kinds the transport can check without running mod code. Any
/// other codec id is treated as opaque bytes: the frame still travels, but
/// only its declared id and direction are validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PayloadCodec {
    /// The codec's bytes are opaque (`minecraft:byte_array`, `minecraft:raw`).
    Raw,
    /// A UTF-8 string (`minecraft:string`, `minecraft:utf8`).
    Utf8,
    /// No payload bytes (`minecraft:unit`, `minecraft:empty`).
    Empty,
}

impl PayloadCodec {
    /// The codec a declaration names, when it is one the transport checks.
    pub(super) fn parse(codec: &str) -> Option<Self> {
        match codec {
            "minecraft:byte_array" | "minecraft:raw" | "minecraft:byte_buffer" => Some(Self::Raw),
            "minecraft:string" | "minecraft:utf8" | "minecraft:utf_8" => Some(Self::Utf8),
            "minecraft:unit" | "minecraft:empty" | "minecraft:void" => Some(Self::Empty),
            _ => None,
        }
    }

    /// Check a frame's bytes against this codec.
    fn validate(self, bytes: &[u8]) -> Result<(), String> {
        match self {
            Self::Raw => Ok(()),
            Self::Utf8 => std::str::from_utf8(bytes)
                .map(|_| ())
                .map_err(|_| "payload codec minecraft:string expects UTF-8 bytes".to_owned()),
            Self::Empty if !bytes.is_empty() => Err(format!(
                "payload codec minecraft:empty expects no bytes, got {}",
                bytes.len()
            )),
            Self::Empty => Ok(()),
        }
    }

    /// Decode a frame's bytes into the value this codec models.
    fn decode(self, bytes: &[u8]) -> Result<PayloadValue, String> {
        check_payload_len(bytes)?;
        match self {
            Self::Raw => Ok(PayloadValue::Bytes(bytes.to_vec())),
            Self::Utf8 => std::str::from_utf8(bytes)
                .map(|text| PayloadValue::Text(text.to_owned()))
                .map_err(|error| {
                    format!("payload codec minecraft:string expects UTF-8 bytes: {error}")
                }),
            Self::Empty if bytes.is_empty() => Ok(PayloadValue::Empty),
            Self::Empty => Err(format!(
                "payload codec minecraft:empty expects no bytes, got {}",
                bytes.len()
            )),
        }
    }
}

/// What a handler could receive from a payload frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PayloadValue {
    /// Opaque bytes (`minecraft:byte_array`).
    Bytes(Vec<u8>),
    /// A UTF-8 string (`minecraft:string`).
    Text(String),
    /// No payload bytes (`minecraft:unit`).
    Empty,
    /// A codec the transport does not model: the first [`MAX_MARKER_BYTES`]
    /// bytes as lowercase hex.
    Marker(String),
}

impl PayloadValue {
    /// Decode a frame's bytes through a declaration's codec id. An id the
    /// transport does not know keeps only its bounded hex.
    pub(super) fn decode(codec: Option<&str>, bytes: &[u8]) -> Result<Self, String> {
        match codec.and_then(PayloadCodec::parse) {
            Some(codec) => codec.decode(bytes),
            None => {
                check_payload_len(bytes)?;
                Ok(Self::Marker(marker_hex(bytes)))
            }
        }
    }
}

/// The bounded lowercase hex a marker spells: the first [`MAX_MARKER_BYTES`]
/// bytes, so even a maximal payload stays small in a log.
pub(super) fn marker_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(MAX_MARKER_BYTES)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn check_payload_len(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(format!(
            "payload value is {} bytes (max {MAX_PAYLOAD_BYTES})",
            bytes.len()
        ));
    }
    Ok(())
}

/// Validate a frame against its declaration's codec, when the codec is one
/// the transport models. Unknown codecs stay opaque.
pub(super) fn validate_payload(
    declaration: &PayloadDeclaration,
    bytes: &[u8],
) -> Result<(), String> {
    match declaration.codec.as_deref().and_then(PayloadCodec::parse) {
        Some(codec) => codec.validate(bytes),
        None => Ok(()),
    }
}

/// A namespaced resource location, bounded and free of control characters so
/// a diagnostic can carry it into a log.
fn check_id(id: &str) -> Result<(), String> {
    let Some((namespace, path)) = id.split_once(':') else {
        return Err(format!("payload id {id:?} has no namespace"));
    };
    let short = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
    };
    if id.len() <= MAX_PAYLOAD_ID
        && !namespace.is_empty()
        && !path.is_empty()
        && short(namespace)
        && path
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_./-".contains(&b))
    {
        return Ok(());
    }
    Err(format!("payload id {id:?} is not a resource location"))
}

/// The declarations one mod bootstrap produced, ordered by id.
///
/// One id names one channel, so the same declaration twice is idempotent and
/// any other repeated id is a conflict: a frame's id and direction must
/// identify exactly one declaration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PayloadRegistry {
    declarations: BTreeMap<String, PayloadDeclaration>,
}

impl PayloadRegistry {
    /// A duplicate is accepted only when identical; a different direction or
    /// codec for the same id is a conflict.
    pub(super) fn register(&mut self, declaration: PayloadDeclaration) -> Result<(), String> {
        declaration.validate()?;
        if let Some(existing) = self.declarations.get(&declaration.id) {
            if existing == &declaration {
                return Ok(());
            }
            return Err(format!(
                "payload {:?} conflicts with its {} declaration (codec {:?})",
                declaration.id,
                existing.direction.name(),
                existing.codec
            ));
        }
        if self.declarations.len() >= MAX_PAYLOAD_DECLARATIONS {
            return Err(format!(
                "more than {MAX_PAYLOAD_DECLARATIONS} payload declarations"
            ));
        }
        self.declarations
            .insert(declaration.id.clone(), declaration);
        Ok(())
    }

    /// Every declaration, sorted by id.
    pub(super) fn declarations(&self) -> impl Iterator<Item = &PayloadDeclaration> {
        self.declarations.values()
    }

    pub(super) fn get(&self, id: &str) -> Option<&PayloadDeclaration> {
        self.declarations.get(id)
    }
}

/// One payload frame: its channel, its id and its bytes.
///
/// The wire format is big-endian: `u32 length` (everything after it), `u8
/// direction`, `u16 id length`, the UTF-8 id, then the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PayloadFrame {
    id: String,
    direction: PayloadDirection,
    bytes: Vec<u8>,
}

impl PayloadFrame {
    /// A frame whose id and payload are inside the model's bounds.
    pub(super) fn new(
        id: &str,
        direction: PayloadDirection,
        bytes: Vec<u8>,
    ) -> Result<Self, String> {
        check_frame_id(id)?;
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "payload frame is {} bytes (max {MAX_PAYLOAD_BYTES})",
                bytes.len()
            ));
        }
        Ok(Self {
            id: id.to_owned(),
            direction,
            bytes,
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn direction(&self) -> PayloadDirection {
        self.direction
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn encode(&self) -> Vec<u8> {
        let rest = 3 + self.id.len() + self.bytes.len();
        debug_assert!(rest <= MAX_FRAME_REST);
        let mut out = Vec::with_capacity(4 + rest);
        out.extend_from_slice(&(rest as u32).to_be_bytes());
        out.push(self.direction.wire());
        out.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
        out.extend_from_slice(self.id.as_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Read exactly one frame from the front of `bytes`, returning it and how
    /// many bytes it used; anything after it is left for the next call.
    pub(super) fn decode(bytes: &[u8]) -> Result<(Self, usize), String> {
        let header = bytes.get(..4).ok_or_else(|| {
            format!(
                "payload frame is {} bytes, shorter than its 4-byte header",
                bytes.len()
            )
        })?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if length > MAX_FRAME_REST {
            return Err(format!(
                "payload frame of {length} bytes exceeds {MAX_FRAME_REST}"
            ));
        }
        let end = 4usize
            .checked_add(length)
            .ok_or_else(|| format!("payload frame length {length} overflows"))?;
        if bytes.len() < end {
            return Err(format!(
                "payload frame is truncated: needs {end} bytes, has {}",
                bytes.len()
            ));
        }
        let (&wire, rest) = bytes[4..end]
            .split_first()
            .ok_or_else(|| "payload frame has no direction byte".to_owned())?;
        let direction = PayloadDirection::from_wire(wire)?;
        let (id_len, body) = rest
            .split_at_checked(2)
            .ok_or_else(|| "payload frame has no id length".to_owned())?;
        let id_len = u16::from_be_bytes([id_len[0], id_len[1]]) as usize;
        if id_len > MAX_PAYLOAD_ID {
            return Err(format!(
                "payload frame id is {id_len} bytes (max {MAX_PAYLOAD_ID})"
            ));
        }
        let (id, payload) = body
            .split_at_checked(id_len)
            .ok_or_else(|| format!("payload frame id is truncated: needs {id_len} bytes"))?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "payload frame carries {} bytes (max {MAX_PAYLOAD_BYTES})",
                payload.len()
            ));
        }
        let id = std::str::from_utf8(id)
            .map_err(|error| format!("payload frame id is not UTF-8: {error}"))?;
        check_frame_id(id)?;
        Ok((
            Self {
                id: id.to_owned(),
                direction,
                bytes: payload.to_vec(),
            },
            end,
        ))
    }
}

/// A frame id is plain bounded text: non-empty, no control characters. The
/// registry's namespaced check is stricter and applies to declarations.
fn check_frame_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("payload frame id is empty".to_owned());
    }
    if id.len() > MAX_PAYLOAD_ID {
        return Err(format!(
            "payload frame id is {} bytes (max {MAX_PAYLOAD_ID})",
            id.len()
        ));
    }
    if id.chars().any(char::is_control) {
        return Err(format!("payload frame id {id:?} has control characters"));
    }
    Ok(())
}

/// A declaration accepts a concrete direction when it names it or is the
/// bidirectional play form.
pub(super) fn accepts(declared: PayloadDirection, direction: PayloadDirection) -> bool {
    declared == direction
        || (declared == PayloadDirection::PlayBidirectional
            && matches!(
                direction,
                PayloadDirection::PlayToClient | PayloadDirection::PlayToServer
            ))
}

// ---------------------------------------------------------------------------
// Chunk envelopes.
// ---------------------------------------------------------------------------

/// The play channel frames travel on.
pub(super) const CHANNEL: &str = "horizon:payload";
/// The channel the server announces its declarations on.
pub(super) const REGISTER_CHANNEL: &str = "horizon:payload_channels";

const MAX_INCOMING: usize = 256;
const MAX_OUTGOING: usize = 256;
const MAX_REGISTRATION: usize = 64 * 1024;
const MAX_DECLARATIONS: usize = 4_096;

/// The most bytes one `horizon:payload` packet may carry: vanilla's
/// `ServerboundCustomPayloadPacket` discards anything above this, in play and
/// in configuration (`horizon_protocol::quiet::MAX_PAYLOAD`). A mod frame
/// bigger than the envelope travels as chunks ([`chunk_envelopes`]).
pub(super) const MAX_ENVELOPE: usize = horizon_protocol::quiet::MAX_PAYLOAD;

/// How many chunks one frame may split into. Each envelope carries at most
/// [`MAX_ENVELOPE`] bytes, so 64 chunks cover the 1 MiB payload model twice
/// over while still bounding a sender's work to one small number.
pub(super) const MAX_CHUNKS: usize = 64;

/// The most bytes a joined frame may hold: the payload model's 1 MiB plus the
/// frame's `u32` length, direction and id header. Chunking never raises the
/// payload cap; it only lets a frame that fits reach the other side.
pub(super) const MAX_JOINED: usize = MAX_PAYLOAD_BYTES + 4 + 3 + MAX_PAYLOAD_ID;

/// The first four bytes of a chunk envelope, the payload model's frame length,
/// or anything the model can produce: a v1 receiver's [`PayloadFrame::decode`]
/// rejects such a length, so a chunk is never mistaken for a frame.
const CHUNK_MAGIC: [u8; 4] = [0xFF, b'H', b'P', b'C'];
/// The chunk envelope version this build writes.
const CHUNK_VERSION: u8 = 1;
/// Magic, version, index, count, and the transfer's total length.
const CHUNK_HEADER: usize = 4 + 1 + 2 + 2 + 4;
/// The bytes of a frame one chunk envelope carries.
const CHUNK_DATA: usize = MAX_ENVELOPE - CHUNK_HEADER;

/// Split one encoded frame into `horizon:payload` envelopes. A frame that
/// fits the envelope comes back as itself, exactly as a v1 sender would send
/// it, so small frames are unchanged on the wire. A bigger one (up to
/// [`MAX_JOINED`]) becomes consecutive chunks an assembler joins back.
pub(super) fn chunk_envelopes(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if data.len() <= MAX_ENVELOPE {
        return Ok(vec![data.to_vec()]);
    }
    if data.len() > MAX_JOINED {
        return Err(format!(
            "payload frame of {} bytes exceeds {MAX_JOINED}",
            data.len()
        ));
    }
    let count = data.len().div_ceil(CHUNK_DATA);
    if count > MAX_CHUNKS {
        return Err(format!(
            "payload frame needs {count} chunks (max {MAX_CHUNKS})"
        ));
    }
    let count = u16::try_from(count).map_err(|_| "chunk count overflows".to_owned())?;
    let total = u32::try_from(data.len())
        .map_err(|_| format!("payload frame of {} bytes overflows", data.len()))?;
    let mut envelopes = Vec::with_capacity(count as usize);
    for (index, chunk) in data.chunks(CHUNK_DATA).enumerate() {
        let mut envelope = Vec::with_capacity(CHUNK_HEADER + chunk.len());
        envelope.extend_from_slice(&CHUNK_MAGIC);
        envelope.push(CHUNK_VERSION);
        envelope.extend_from_slice(&(index as u16).to_be_bytes());
        envelope.extend_from_slice(&count.to_be_bytes());
        envelope.extend_from_slice(&total.to_be_bytes());
        envelope.extend_from_slice(chunk);
        debug_assert!(envelope.len() <= MAX_ENVELOPE);
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

/// Whether an envelope is a chunk rather than a whole frame. A frame's first
/// four bytes are its length and can never be [`CHUNK_MAGIC`].
pub(super) fn is_chunk_envelope(data: &[u8]) -> bool {
    data.len() >= CHUNK_HEADER && data.starts_with(&CHUNK_MAGIC) && data[4] == CHUNK_VERSION
}

/// Joins the chunks of consecutive oversized frames on one connection. The
/// stream is ordered, so chunks of one frame arrive in order; a chunk out of
/// order, a restart mid-transfer or a transfer past its declared total
/// rejects, and the assembler starts over.
#[derive(Debug, Default)]
pub(super) struct ChunkAssembler {
    count: u16,
    next: u16,
    total: u32,
    data: Vec<u8>,
}

impl ChunkAssembler {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// No transfer is in progress.
    pub(super) fn is_idle(&self) -> bool {
        self.next == 0
    }

    /// Feed one chunk envelope. `Ok(None)` means more chunks are due;
    /// `Ok(Some(frame))` is the frame the chunks spelled.
    pub(super) fn push(&mut self, envelope: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if !is_chunk_envelope(envelope) {
            return Err("payload chunk is not a chunk envelope".to_owned());
        }
        let index = u16::from_be_bytes([envelope[5], envelope[6]]);
        let count = u16::from_be_bytes([envelope[7], envelope[8]]);
        let total = u32::from_be_bytes([envelope[9], envelope[10], envelope[11], envelope[12]]);
        if count == 0 || usize::from(count) > MAX_CHUNKS {
            return Err(format!("payload frame declares {count} chunks"));
        }
        if usize::try_from(total).map_or(true, |total| total > MAX_JOINED) {
            return Err(format!("payload frame declares {total} bytes"));
        }
        if index >= count {
            return Err(format!("payload chunk {index} of {count} is out of range"));
        }
        if index == 0 {
            // A fresh transfer replaces an incomplete one, so a dropped tail
            // cannot wedge the connection.
            self.count = count;
            self.next = 0;
            self.total = total;
            self.data.clear();
            self.data.reserve(usize::try_from(total).unwrap_or(0));
        }
        if count != self.count || total != self.total || index != self.next {
            let due = self.next;
            *self = Self::default();
            return Err(format!(
                "payload chunk {index} arrived where chunk {due} was due"
            ));
        }
        let chunk = &envelope[CHUNK_HEADER..];
        let total = usize::try_from(total).unwrap_or(0);
        if self.data.len() + chunk.len() > total {
            *self = Self::default();
            return Err(format!("payload chunks exceed their {total} bytes"));
        }
        self.data.extend_from_slice(chunk);
        self.next += 1;
        if self.next < self.count {
            return Ok(None);
        }
        if self.data.len() != total {
            *self = Self::default();
            return Err(format!(
                "payload frame is {} bytes, not its {total}",
                self.data.len()
            ));
        }
        let joined = std::mem::take(&mut self.data);
        *self = Self::default();
        Ok(Some(joined))
    }
}

// ---------------------------------------------------------------------------
// One connection's declarations and frames.
// ---------------------------------------------------------------------------

/// One connection's payload state: the declarations both sides agreed on,
/// received frames waiting for the game, and encoded frames waiting to be
/// sent.
#[derive(Clone, Debug, Default)]
pub(super) struct PayloadLink {
    registry: PayloadRegistry,
    incoming: Vec<PayloadFrame>,
    outgoing: Vec<Vec<u8>>,
}

impl PayloadLink {
    pub(super) fn new(registry: PayloadRegistry) -> Self {
        Self {
            registry,
            incoming: Vec::new(),
            outgoing: Vec::new(),
        }
    }

    /// A link over the declarations the last content root recorded.
    pub(super) fn from_declared() -> Self {
        Self::new(declared())
    }

    pub(super) fn registry(&self) -> &PayloadRegistry {
        &self.registry
    }

    /// The declarations as they travel to the other side. `u16` count, then
    /// per declaration: `u16` id length, id, direction byte, `u8` has-codec,
    /// and (with codec) `u16` length and text. Bounded to
    /// [`MAX_REGISTRATION`].
    pub(super) fn registration(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Reserve the count until we know how many complete declarations fit:
        // truncating a finished buffer could cut a length-prefixed id in half.
        out.extend_from_slice(&0u16.to_be_bytes());
        let mut count = 0u16;
        for declaration in self.registry.declarations().take(u16::MAX as usize) {
            let mut encoded = Vec::new();
            encoded.extend_from_slice(&(declaration.id.len() as u16).to_be_bytes());
            encoded.extend_from_slice(declaration.id.as_bytes());
            encoded.push(declaration.direction.wire());
            match &declaration.codec {
                Some(codec) => {
                    encoded.push(1);
                    encoded.extend_from_slice(&(codec.len() as u16).to_be_bytes());
                    encoded.extend_from_slice(codec.as_bytes());
                }
                None => encoded.push(0),
            }
            if out.len() + encoded.len() > MAX_REGISTRATION {
                break;
            }
            out.extend_from_slice(&encoded);
            count += 1;
        }
        out[..2].copy_from_slice(&count.to_be_bytes());
        out
    }

    /// Apply the other side's declaration list; returns how many channels it
    /// registered. Replaces the local registry.
    pub(super) fn apply_registration(&mut self, data: &[u8]) -> Result<usize, String> {
        if data.len() > MAX_REGISTRATION {
            return Err(format!("payload registration is {} bytes", data.len()));
        }
        let mut reader = Reader { bytes: data, at: 0 };
        let count = reader.u16()? as usize;
        if count > MAX_DECLARATIONS {
            return Err(format!("payload registration lists {count} channels"));
        }
        let mut registry = PayloadRegistry::default();
        for _ in 0..count {
            let id = reader.text()?;
            let direction = PayloadDirection::from_wire(reader.u8()?)?;
            let codec = match reader.u8()? {
                0 => None,
                1 => Some(reader.text()?),
                other => return Err(format!("payload codec flag {other} is not 0 or 1")),
            };
            registry.register(PayloadDeclaration {
                id,
                direction,
                codec,
            })?;
        }
        if reader.at != data.len() {
            return Err("trailing bytes after payload registration".to_owned());
        }
        self.registry = registry;
        Ok(count)
    }

    /// Decode one frame from the other side, accepting it only when its id is
    /// declared, `expected` matches its direction (or a bidirectional
    /// declaration), and its codec validates the bytes.
    pub(super) fn receive(
        &mut self,
        data: &[u8],
        expected: PayloadDirection,
    ) -> Result<&PayloadFrame, String> {
        let (frame, used) = PayloadFrame::decode(data)?;
        if used != data.len() {
            return Err(format!(
                "payload packet carries {} trailing bytes",
                data.len() - used
            ));
        }
        let declaration = self
            .registry
            .get(frame.id())
            .ok_or_else(|| format!("payload channel {} is not declared", frame.id()))?;
        if !accepts(declaration.direction, frame.direction()) || frame.direction() != expected {
            return Err(format!(
                "payload {} travels {} but was received as {}",
                frame.id(),
                frame.direction().name(),
                expected.name()
            ));
        }
        validate_payload(declaration, frame.bytes())?;
        if self.incoming.len() >= MAX_INCOMING {
            return Err("payload receive queue is full".to_owned());
        }
        self.incoming.push(frame);
        Ok(self.incoming.last().expect("just pushed"))
    }

    /// Encode a frame for the other side, validating the declaration, the
    /// direction and the supported codec first. The encoded bytes are also
    /// queued for [`take_outgoing`](Self::take_outgoing).
    pub(super) fn send(
        &mut self,
        id: &str,
        direction: PayloadDirection,
        bytes: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        let declaration = self
            .registry
            .get(id)
            .ok_or_else(|| format!("payload channel {id} is not declared"))?;
        if !accepts(declaration.direction, direction) {
            return Err(format!(
                "payload {id} is declared {} not {}",
                declaration.direction.name(),
                direction.name()
            ));
        }
        validate_payload(declaration, &bytes)?;
        let encoded = PayloadFrame::new(id, direction, bytes)?.encode();
        if self.outgoing.len() >= MAX_OUTGOING {
            return Err("payload send queue is full".to_owned());
        }
        self.outgoing.push(encoded.clone());
        Ok(encoded)
    }

    pub(super) fn take_incoming(&mut self) -> Vec<PayloadFrame> {
        std::mem::take(&mut self.incoming)
    }

    pub(super) fn take_outgoing(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outgoing)
    }

    /// Take at most `max` received frames, oldest first, and keep the rest
    /// queued: a per-tick drain bounds its work without losing frames.
    pub(super) fn take_incoming_up_to(&mut self, max: usize) -> Vec<PayloadFrame> {
        let count = max.min(self.incoming.len());
        self.incoming.drain(..count).collect()
    }

    pub(super) fn incoming(&self) -> &[PayloadFrame] {
        &self.incoming
    }

    pub(super) fn outgoing(&self) -> &[Vec<u8>] {
        &self.outgoing
    }
}

/// A tiny big-endian reader for the registration list.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| "payload registration is truncated".to_owned())?;
        self.at += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let hi = self.u8()?;
        let lo = self.u8()?;
        Ok(u16::from_be_bytes([hi, lo]))
    }

    fn text(&mut self) -> Result<String, String> {
        let len = self.u16()? as usize;
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| "payload registration length overflows".to_owned())?;
        let text = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| "payload registration is truncated".to_owned())?;
        self.at = end;
        String::from_utf8(text.to_vec())
            .map_err(|_| "payload registration text is not UTF-8".to_owned())
    }
}

// ---------------------------------------------------------------------------
// Java handlers.
// ---------------------------------------------------------------------------

/// The jars and registrations of running Java mods, so a received frame can
/// reach its registered handler.
struct HandlerJar {
    jar: NativeJar,
    registrations: Vec<PayloadRegistration>,
    /// The host the jar bootstrapped on, when the caller kept it. Its
    /// payload-type markers are what let a registration's marker resolve to
    /// a channel id; a handler jar stored without one never matches a frame.
    host: Option<StdHost>,
}

/// The execution role whose Java payload handlers own a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum HandlerRole {
    Client,
    IntegratedServer,
    DedicatedServer,
}

static HANDLER_JARS: Mutex<BTreeMap<HandlerRole, Vec<HandlerJar>>> = Mutex::new(BTreeMap::new());

/// Remember one mod jar's payload registrations; called after its bootstrap.
#[allow(dead_code)] // the bootstrap stores the host form below
pub(super) fn store_handler_jar(jar: NativeJar, registrations: Vec<PayloadRegistration>) {
    store_handler_jar_as(HandlerRole::Client, jar, registrations);
}

/// Remember one mod jar's payload registrations for a specific execution role.
pub(super) fn store_handler_jar_as(
    role: HandlerRole,
    jar: NativeJar,
    registrations: Vec<PayloadRegistration>,
) {
    if registrations.is_empty() {
        return;
    }
    HANDLER_JARS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(role)
        .or_default()
        .push(HandlerJar {
            jar,
            registrations,
            host: None,
        });
}

/// [`store_handler_jar`] that also keeps the bootstrapped host, so a received
/// frame's channel id resolves to the registration's payload-type marker.
/// Registrations come from the host itself. Sends the host had queued are
/// dropped from the kept copy (the caller already published them).
pub(super) fn store_handler_host(jar: NativeJar, host: &StdHost) {
    store_handler_host_as(HandlerRole::Client, jar, host);
}

/// [`store_handler_host`] for a specific execution role.
pub(super) fn store_handler_host_as(role: HandlerRole, jar: NativeJar, host: &StdHost) {
    let registrations = host.payload_registrations().to_vec();
    if registrations.is_empty() {
        return;
    }
    let mut template = host.clone();
    let _ = template.take_outgoing_payloads();
    let _ = template.take_incoming_payloads();
    HANDLER_JARS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(role)
        .or_default()
        .push(HandlerJar {
            jar,
            registrations,
            host: Some(template),
        });
}

/// Forget the jars of a previous bootstrap.
pub(super) fn clear_handler_jars() {
    HANDLER_JARS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Forget only the jars belonging to one execution role.
pub(super) fn clear_handler_jars_as(role: HandlerRole) {
    HANDLER_JARS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&role);
}

/// Dispatch one received frame to the first registered handler that names its
/// channel. Returns whether a handler ran; sends it queues are published for
/// the next server tick.
#[allow(dead_code)] // the hub passes its execution role explicitly
pub(super) fn dispatch_received(frame: &PayloadFrame) -> Result<bool, String> {
    dispatch_received_as_role(
        frame,
        PayloadDirection::PlayToServer,
        HandlerRole::IntegratedServer,
    )
}

/// [`dispatch_received`] for the side that received the frame: only
/// registrations that receive in `receiving` (their direction, or a
/// bidirectional one) can handle it. A client window passes `PlayToClient`,
/// so a server-side receiver of the same channel never answers its frames.
pub(super) fn dispatch_received_as(
    frame: &PayloadFrame,
    receiving: PayloadDirection,
) -> Result<bool, String> {
    let role = match receiving {
        PayloadDirection::PlayToClient => HandlerRole::Client,
        PayloadDirection::PlayToServer => HandlerRole::IntegratedServer,
        PayloadDirection::PlayBidirectional
        | PayloadDirection::ConfigurationToClient
        | PayloadDirection::ConfigurationToServer => HandlerRole::Client,
    };
    dispatch_received_as_role(frame, receiving, role)
}

/// [`dispatch_received_as`] with an explicit execution role.
pub(super) fn dispatch_received_as_role(
    frame: &PayloadFrame,
    receiving: PayloadDirection,
    role: HandlerRole,
) -> Result<bool, String> {
    dispatch_matching(frame, Some(receiving), role)
}

/// Whether a registration's recorded direction receives frames travelling in
/// `receiving`. An unparseable direction (configuration phase) never does.
fn registration_receives(direction: &str, receiving: PayloadDirection) -> bool {
    match PayloadDirection::parse(direction) {
        Ok(PayloadDirection::PlayBidirectional) => true,
        Ok(direction) => direction == receiving,
        Err(_) => false,
    }
}

fn dispatch_matching(
    frame: &PayloadFrame,
    receiving: Option<PayloadDirection>,
    role: HandlerRole,
) -> Result<bool, String> {
    let declarations = declared();
    let declaration = declarations.get(frame.id());
    let (payload, context) = payload_values(declaration, frame);
    let handlers = HANDLER_JARS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handlers) = handlers.get(&role) else {
        return Ok(false);
    };
    for handler in handlers.iter() {
        let registrations: Vec<PayloadRegistration> = handler
            .registrations
            .iter()
            .filter(|registration| {
                receiving.is_none_or(|direction| {
                    registration_receives(&registration.direction, direction)
                })
            })
            .cloned()
            .collect();
        let mut host = handler.host.clone().unwrap_or_else(StdHost::new);
        let matched = dispatch_payload_frame_with(
            &handler.jar,
            &mut host,
            &registrations,
            frame.id(),
            payload.clone(),
            context.clone(),
        )
        .map_err(|error| error.to_string())?;
        if matched {
            publish_from_host(&mut host)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// The payload and context a handler receives. A supported codec gives it the
/// decoded string, an empty marker or the bounded bytes marker; a codec the
/// transport does not model stays the bounded `payload/<id>/<hex>` marker.
fn payload_values(
    declaration: Option<&PayloadDeclaration>,
    frame: &PayloadFrame,
) -> (Value, Value) {
    let id = frame.id();
    let context = Value::Ref(Some(format!("payload-context/{id}")));
    let codec = declaration.and_then(|declaration| declaration.codec.as_deref());
    let value = PayloadValue::decode(codec, frame.bytes())
        .unwrap_or_else(|_| PayloadValue::Marker(marker_hex(frame.bytes())));
    (payload_value(&value, id), context)
}

/// The native host has no byte-array value, so a raw payload crosses as its
/// bounded hex. `payload-bytes/<hex>` keeps it distinct from the
/// `payload/<id>/<hex>` marker of an unmodelled codec; both spell at most
/// [`MAX_MARKER_BYTES`] bytes.
fn payload_value(value: &PayloadValue, id: &str) -> Value {
    match value {
        PayloadValue::Text(text) => Value::Ref(Some(text.clone())),
        PayloadValue::Empty => Value::Ref(Some(format!("payload-empty/{id}"))),
        PayloadValue::Bytes(bytes) => {
            Value::Ref(Some(format!("payload-bytes/{}", marker_hex(bytes))))
        }
        PayloadValue::Marker(hex) => Value::Ref(Some(format!("payload/{id}/{hex}"))),
    }
}

// ---------------------------------------------------------------------------
// Frames Java mods send, and the declarations a content root recorded.
// ---------------------------------------------------------------------------

/// Frames a Java host asked to send, waiting for the server's next tick.
static JAVA_OUTGOING: Mutex<Vec<PayloadFrame>> = Mutex::new(Vec::new());

/// The `PlayToServer` frames of [`JAVA_OUTGOING`], mirrored for a client
/// window. The hub drains (and ignores) every frame of the shared queue each
/// tick, so a window cannot read its sends from there without racing it in an
/// integrated game; this copy is only ever drained by
/// [`take_java_to_server`]. A process that never drains it (a dedicated
/// server) drops the oldest frames instead of failing sends.
static JAVA_TO_SERVER: Mutex<Vec<PayloadFrame>> = Mutex::new(Vec::new());

/// Translate a Java host's queued sends into validated frames. A channel must
/// be declared, the direction must match, and a supported codec must accept
/// the bytes; the whole batch stops at the first bad frame.
pub(super) fn publish_from_host_with(
    registry: &PayloadRegistry,
    host: &mut StdHost,
) -> Result<Vec<PayloadFrame>, String> {
    let mut frames = Vec::new();
    for (id, direction, bytes) in host.take_outgoing_payloads() {
        let direction = PayloadDirection::parse(&direction)?;
        let declaration = registry
            .get(&id)
            .ok_or_else(|| format!("payload channel {id} is not declared"))?;
        if !accepts(declaration.direction, direction) {
            return Err(format!(
                "payload {id} is declared {} not {}",
                declaration.direction.name(),
                direction.name()
            ));
        }
        validate_payload(declaration, &bytes)?;
        frames.push(PayloadFrame::new(&id, direction, bytes)?);
    }
    Ok(frames)
}

/// [`publish_from_host_with`] against the last stored declarations, queueing
/// the frames for the server's next tick. Returns how many were published.
pub(super) fn publish_from_host(host: &mut StdHost) -> Result<usize, String> {
    let frames = publish_from_host_with(&declared(), host)?;
    let count = frames.len();
    if !frames.is_empty() {
        let mut queue = JAVA_OUTGOING.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() + frames.len() > MAX_OUTGOING {
            return Err("Java payload send queue is full".to_owned());
        }
        queue.extend(frames.iter().cloned());
        drop(queue);
        let mut mirror = JAVA_TO_SERVER.lock().unwrap_or_else(|e| e.into_inner());
        mirror.extend(
            frames
                .into_iter()
                .filter(|frame| frame.direction() == PayloadDirection::PlayToServer),
        );
        if mirror.len() > MAX_OUTGOING {
            let excess = mirror.len() - MAX_OUTGOING;
            mirror.drain(..excess);
        }
    }
    Ok(count)
}

/// Frames Java mods asked to send, drained in order.
#[allow(dead_code)] // the hub drains the client-bound half through `take_java_to_clients`
pub(super) fn take_java_outgoing() -> Vec<PayloadFrame> {
    std::mem::take(&mut JAVA_OUTGOING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Take at most `max` frames Java mods addressed to the server, oldest first;
/// the rest stay queued for the next call. Each frame is returned once.
pub(super) fn take_java_to_server(max: usize) -> Vec<PayloadFrame> {
    let mut queue = JAVA_TO_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let count = max.min(queue.len());
    queue.drain(..count).collect()
}

/// Take at most `max` frames Java mods addressed to a client (`PlayToClient`,
/// or the bidirectional play form), oldest first; the rest stay queued for
/// the next call. A server-bound frame is not this queue's to deliver: the
/// client window reads it from [`JAVA_TO_SERVER`] (`take_java_to_server`), so
/// it is never returned here and never broadcast by the hub.
pub(super) fn take_java_to_clients(max: usize) -> Vec<PayloadFrame> {
    let mut queue = JAVA_OUTGOING.lock().unwrap_or_else(|e| e.into_inner());
    let mut taken = Vec::new();
    let mut kept = Vec::with_capacity(queue.len());
    for frame in queue.drain(..) {
        let client_bound = matches!(
            frame.direction(),
            PayloadDirection::PlayToClient | PayloadDirection::PlayBidirectional
        );
        if !client_bound {
            // Its copy is already in the mirror for the client window.
            continue;
        }
        if taken.len() < max {
            taken.push(frame);
        } else {
            kept.push(frame);
        }
    }
    *queue = kept;
    taken
}

/// Hand received frames to a Java host for its bootstrap/bootstrap-time view.
/// Returns how many were delivered.
pub(super) fn deliver_to_host(
    host: &mut StdHost,
    frames: impl IntoIterator<Item = PayloadFrame>,
) -> usize {
    let mut count = 0;
    for frame in frames {
        host.deliver_payload(frame.id(), frame.bytes().to_vec());
        count += 1;
    }
    count
}

/// The declarations of the last content root, for links built later.
static DECLARED: Mutex<Option<PayloadRegistry>> = Mutex::new(None);

/// Record the channels a content root declared. A repeated id with a
/// different direction or codec is a conflict, as at bootstrap.
pub(super) fn store_declared(
    declarations: impl IntoIterator<Item = PayloadDeclaration>,
) -> Result<usize, String> {
    let mut registry = PayloadRegistry::default();
    let mut count = 0;
    for declaration in declarations {
        registry.register(declaration)?;
        count += 1;
    }
    *DECLARED.lock().unwrap_or_else(|e| e.into_inner()) = Some(registry.clone());
    // Frames queued by a previous content root must not be sent using the
    // replacement registry. Drop them atomically with the declaration swap.
    JAVA_OUTGOING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    JAVA_TO_SERVER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    // A guest holds the list it was last sent; a replaced content root sends
    // its own on the next tick, to every guest again.
    for link in GUEST_LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        link.registration_sent = false;
        link.link = PayloadLink::new(registry.clone());
    }
    Ok(count)
}

/// The last stored declarations; empty until a content root records some.
pub(super) fn declared() -> PayloadRegistry {
    DECLARED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The declared channels keyed by id, for diagnostics.
pub(super) fn declared_by_id() -> BTreeMap<String, PayloadDeclaration> {
    declared()
        .declarations()
        .map(|declaration| (declaration.id.clone(), declaration.clone()))
        .collect()
}

/// The declarations a bootstrapped Java host announces, as the transport's
/// model. A channel the model cannot parse is reported, so the caller can log
/// the one channel it skipped instead of failing the whole bootstrap.
pub(super) fn declarations_from_host(host: &StdHost) -> (Vec<PayloadDeclaration>, Vec<String>) {
    let mut declarations = Vec::new();
    let mut skipped = Vec::new();
    for (id, direction, codec) in host.payload_channels() {
        match PayloadDirection::parse(&direction) {
            Ok(direction) => declarations.push(PayloadDeclaration {
                id,
                direction,
                codec,
            }),
            Err(error) => skipped.push(format!("{id}: {error}")),
        }
    }
    (declarations, skipped)
}

// ---------------------------------------------------------------------------
// The hub side: guests' frames, and the declarations they were sent.
// ---------------------------------------------------------------------------

/// Frames a tick at most in each direction, bounding the work one tick spends
/// on Java payloads. The client window's pump and the hub share the bound.
pub(super) const PAYLOAD_TICK_CAP: usize = 256;

/// One guest's server-side payload state.
struct GuestLink {
    link: PayloadLink,
    /// Joins the chunks of an oversized frame. The network is ordered, so one
    /// transfer is in flight at a time.
    chunks: ChunkAssembler,
    /// The guest has the host's declaration list.
    registration_sent: bool,
}

impl GuestLink {
    fn new() -> Self {
        Self {
            link: PayloadLink::from_declared(),
            chunks: ChunkAssembler::new(),
            registration_sent: false,
        }
    }

    /// One `horizon:payload` packet from this guest, joined when chunked.
    /// `Ok(false)` means more chunks of a frame are due.
    fn receive(&mut self, data: &[u8]) -> Result<bool, String> {
        if !is_chunk_envelope(data) {
            // A whole frame cannot sit between the chunks of a transfer: if
            // one arrives, the sender started over and the chunks are stale.
            if !self.chunks.is_idle() {
                self.chunks = ChunkAssembler::new();
                return Err("a whole frame arrived mid-transfer".to_owned());
            }
            self.link.receive(data, PayloadDirection::PlayToServer)?;
            return Ok(true);
        }
        let joined = match self.chunks.push(data) {
            Ok(Some(joined)) => joined,
            Ok(None) => return Ok(false),
            Err(error) => {
                self.chunks = ChunkAssembler::new();
                return Err(error);
            }
        };
        if let Err(error) = self.link.receive(&joined, PayloadDirection::PlayToServer) {
            self.chunks = ChunkAssembler::new();
            return Err(error);
        }
        Ok(true)
    }
}

/// The guests' payload state, per connection. The hub's per-connection state
/// lives in `lan::hub`, which this workstream does not own, so the transport
/// keeps its own map keyed by the connection id and prunes it as guests leave.
static GUEST_LINKS: Mutex<BTreeMap<u32, GuestLink>> = Mutex::new(BTreeMap::new());

/// Forget a connection's payload state (a guest that left).
pub(super) fn forget_guest(conn: u32) {
    GUEST_LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conn);
}

/// [`store_declared`] from the channels a jar's host announced, in the order
/// the transport needs them at bootstrap: the jar's channels join
/// `declarations` and are stored first (so a frame its init queued is checked
/// against a live registry), the frames it queued are published for the next
/// tick, and the host itself is kept so a received frame reaches the handler it
/// registered.
///
/// Call it once per jar, in load order, after its entrypoints ran. Returns how
/// many channels the content root declares so far; a channel the model cannot
/// parse, a duplicate id and a bad queued frame are warnings, never a startup
/// failure.
pub(super) fn adopt_host(
    role: HandlerRole,
    jar: &NativeJar,
    host: &mut StdHost,
    declarations: &mut Vec<PayloadDeclaration>,
) -> usize {
    let (own, skipped) = declarations_from_host(host);
    for skipped in &skipped {
        log::warn!("Horizon: a Java payload channel was skipped: {skipped}");
    }
    declarations.extend(own);
    let count = match store_declared(declarations.iter().cloned()) {
        Ok(count) => count,
        Err(error) => {
            log::warn!("Horizon: Java payload channels were not registered: {error}");
            declared().declarations().count()
        }
    };
    if let Err(error) = publish_from_host(host) {
        log::warn!("Horizon: a Java payload send at bootstrap was skipped: {error}");
    }
    // Keep the bootstrapped host, not just its registrations: dispatch
    // resolves a registration's payload-type marker through the host's
    // channel map, and a jar stored without one never matches a frame.
    store_handler_host_as(role, jar.clone(), host);
    count
}

/// The `horizon:payload` packets one encoded body travels as: itself when it
/// fits the envelope, and its consecutive chunks when it does not.
fn payload_packets(data: Vec<u8>) -> Result<Vec<C>, String> {
    Ok(chunk_envelopes(&data)?
        .into_iter()
        .map(|data| {
            C::CustomPayload(CustomPayload {
                channel: CHANNEL.to_owned(),
                data,
            })
        })
        .collect())
}

/// The `horizon:payload_channels` packets the host's declarations travel as.
fn registration_packets(data: Vec<u8>) -> Result<Vec<C>, String> {
    Ok(chunk_envelopes(&data)?
        .into_iter()
        .map(|data| {
            C::CustomPayload(CustomPayload {
                channel: REGISTER_CHANNEL.to_owned(),
                data,
            })
        })
        .collect())
}

/// Send one declared frame to every guest placed in the level. Java mods'
/// client-bound sends go out this way, once a tick, from `flush_native_net`.
pub(super) fn broadcast(host: &LanHost, frame: &PayloadFrame) {
    let direction = frame.direction();
    if !matches!(
        direction,
        PayloadDirection::PlayToClient | PayloadDirection::PlayBidirectional
    ) {
        return;
    }
    let mut link = PayloadLink::from_declared();
    let data = match link.send(frame.id(), direction, frame.bytes().to_vec()) {
        Ok(data) => data,
        Err(error) => {
            log::warn!("Horizon: mod payload {} was not sent: {error}", frame.id());
            return;
        }
    };
    // The frame goes out right here; the link's own copy would only pile up
    // against its queue cap.
    link.take_outgoing();
    let packets = match payload_packets(data) {
        Ok(packets) => packets,
        Err(error) => {
            log::warn!("Horizon: mod payload {} was not sent: {error}", frame.id());
            return;
        }
    };
    // A frame's chunks go out together and in order: interleaving two frames
    // would leave a guest's assembler a transfer it must reset.
    for g in host.hub.guests.iter().filter(|g| host.hub.here(g)) {
        g.send_all(&packets);
    }
}

/// Send the host's declaration list to every placed guest that lacks it.
fn send_registrations(host: &LanHost) {
    let registration = PayloadLink::from_declared().registration();
    let mut links = GUEST_LINKS.lock().unwrap_or_else(|e| e.into_inner());
    // A connection that left takes its state with it.
    links.retain(|conn, _| host.hub.guest(*conn).is_some());
    let due: Vec<u32> = host
        .hub
        .guests
        .iter()
        .filter(|g| host.hub.here(g))
        .map(|g| g.conn)
        .filter(|conn| links.get(conn).is_some_and(|link| !link.registration_sent))
        .collect();
    if due.is_empty() {
        return;
    }
    let packets = match registration_packets(registration) {
        Ok(packets) => packets,
        Err(error) => {
            log::warn!("Horizon: mod payload registration was too large to send: {error}");
            return;
        }
    };
    for conn in due {
        if let Some(g) = host.hub.guest(conn) {
            g.send_all(&packets);
        }
        if let Some(link) = links.get_mut(&conn) {
            link.registration_sent = true;
        }
    }
}

impl Game<'_> {
    /// A `horizon:payload` packet from guest `conn`: joined back from its
    /// chunks when it is one, validated against the declared channels, and
    /// queued for [`Self::tick_java_payloads`]. A bad frame is dropped with a
    /// warning; it never closes the connection.
    pub(super) fn guest_java_payload(&mut self, host: &LanHost, conn: u32, data: &[u8]) {
        let mut links = GUEST_LINKS.lock().unwrap_or_else(|e| e.into_inner());
        let guest = links.entry(conn).or_insert_with(GuestLink::new);
        if let Err(error) = guest.receive(data) {
            let name = host
                .hub
                .guest(conn)
                .map_or_else(|| conn.to_string(), |g| g.name.clone());
            log::warn!("Horizon: {name} sent an invalid mod payload: {error}");
        }
    }

    /// The tick's Java payload work for the hub's guests: the declarations
    /// each placed guest lacks, then up to [`PAYLOAD_TICK_CAP`] received
    /// frames per guest to their Java handlers. A remainder stays queued, and
    /// a frame no handler claims is consumed, not retried.
    pub(super) fn tick_java_payloads(&mut self, host: &mut LanHost) {
        send_registrations(host);
        let role = if self.options.dedicated.is_some() {
            HandlerRole::DedicatedServer
        } else {
            HandlerRole::IntegratedServer
        };
        let conns: Vec<u32> = host
            .hub
            .guests
            .iter()
            .filter(|g| host.hub.here(g))
            .map(|g| g.conn)
            .collect();
        // The frames leave the lock before a handler runs: dispatch takes the
        // handler store and hands the handler's sends to the outgoing queue.
        let mut due = Vec::new();
        {
            let mut links = GUEST_LINKS.lock().unwrap_or_else(|e| e.into_inner());
            for conn in conns {
                let Some(link) = links.get_mut(&conn) else {
                    continue;
                };
                due.extend(link.link.take_incoming_up_to(PAYLOAD_TICK_CAP));
            }
        }
        for frame in due {
            match dispatch_received_as_role(&frame, PayloadDirection::PlayToServer, role) {
                Ok(true) => {}
                Ok(false) => log::warn!("Horizon: no Java handler for payload {}", frame.id()),
                Err(error) => log::warn!(
                    "Horizon: Java payload {} handler failed: {error}",
                    frame.id()
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The client window's side.
// ---------------------------------------------------------------------------

/// What one tick of [`pump_client_payloads`] did, for the caller to log and
/// send. Ids are the channel ids of the frames concerned.
#[derive(Debug, Default)]
pub(super) struct PayloadPump {
    /// Received frames a Java handler ran for.
    pub(super) dispatched: usize,
    /// Received frames no handler claimed.
    pub(super) unhandled: Vec<String>,
    /// Received frames whose handler errored: `(id, error)`.
    pub(super) failed: Vec<(String, String)>,
    /// Frames Java mods queued that were refused before sending.
    pub(super) rejected: Vec<(String, String)>,
    /// Encoded `horizon:payload` bodies to send to the server, in order: a
    /// frame that fits the packet envelope is one body, and a bigger one is
    /// its consecutive chunks ([`chunk_envelopes`]).
    pub(super) encoded: Vec<Vec<u8>>,
}

/// One `horizon:payload` packet from the server. A frame within the packet
/// envelope goes to `link` exactly as a v1 sender's does; a bigger one arrives
/// as consecutive chunks ([`chunk_envelopes`]) that `chunks` joins back.
/// `Ok(false)` means more chunks are due.
///
/// A malformed, out-of-order or interrupted chunk sequence rejects and resets
/// the assembler, so the next frame starts clean instead of wedging the
/// connection, and a frame the link itself refuses (undeclared channel, bad
/// codec) takes its transfer down with it.
pub(super) fn receive_payload(
    link: &mut PayloadLink,
    chunks: &mut ChunkAssembler,
    data: &[u8],
) -> Result<bool, String> {
    if !is_chunk_envelope(data) {
        if !chunks.is_idle() {
            *chunks = ChunkAssembler::new();
            return Err("a whole frame arrived mid-transfer".to_owned());
        }
        link.receive(data, PayloadDirection::PlayToClient)?;
        return Ok(true);
    }
    let joined = match chunks.push(data) {
        Ok(Some(joined)) => joined,
        Ok(None) => return Ok(false),
        Err(error) => {
            *chunks = ChunkAssembler::new();
            return Err(error);
        }
    };
    if let Err(error) = link.receive(&joined, PayloadDirection::PlayToClient) {
        *chunks = ChunkAssembler::new();
        return Err(error);
    }
    Ok(true)
}

/// One channel-registration packet from the server. Small registrations are
/// applied directly; oversized ones use the same ordered chunk envelopes as
/// payload frames, with an assembler dedicated to this channel.
pub(super) fn receive_registration(
    link: &mut PayloadLink,
    chunks: &mut ChunkAssembler,
    data: &[u8],
) -> Result<Option<usize>, String> {
    if !is_chunk_envelope(data) {
        if !chunks.is_idle() {
            *chunks = ChunkAssembler::new();
            return Err("a whole registration arrived mid-transfer".to_owned());
        }
        return link.apply_registration(data).map(Some);
    }
    let joined = match chunks.push(data) {
        Ok(Some(joined)) => joined,
        Ok(None) => return Ok(None),
        Err(error) => {
            *chunks = ChunkAssembler::new();
            return Err(error);
        }
    };
    match link.apply_registration(&joined) {
        Ok(count) => Ok(Some(count)),
        Err(error) => {
            *chunks = ChunkAssembler::new();
            Err(error)
        }
    }
}

/// One client tick of mod payload traffic over `link`. First up to `cap`
/// received frames go to `dispatch` (their receivers may queue sends), then up
/// to `send_cap` server-bound frames from `outgoing` are checked against the
/// server's declarations and encoded. Both directions are bounded and leave a
/// remainder queued; a frame that no handler claims is consumed, not retried.
pub(super) fn pump_client_payloads(
    link: &mut PayloadLink,
    cap: usize,
    send_cap: usize,
    dispatch: &mut dyn FnMut(&PayloadFrame) -> Result<bool, String>,
    outgoing: &mut dyn FnMut(usize) -> Vec<PayloadFrame>,
) -> PayloadPump {
    let mut pump = PayloadPump::default();
    for frame in link.take_incoming_up_to(cap) {
        match dispatch(&frame) {
            Ok(true) => pump.dispatched += 1,
            Ok(false) => pump.unhandled.push(frame.id().to_owned()),
            Err(error) => pump.failed.push((frame.id().to_owned(), error)),
        }
    }
    if send_cap == 0 {
        return pump;
    }
    for frame in outgoing(send_cap) {
        // A client only originates frames that travel to the server.
        let result = if matches!(
            frame.direction(),
            PayloadDirection::PlayToServer | PayloadDirection::PlayBidirectional
        ) {
            link.send(frame.id(), frame.direction(), frame.bytes().to_vec())
        } else {
            Err(format!(
                "{} is not a server-bound frame",
                frame.direction().name()
            ))
        };
        match result {
            // A frame above the packet envelope travels as consecutive chunks;
            // the chunks spell the frame's own bytes, in order.
            Ok(data) => match chunk_envelopes(&data) {
                Ok(envelopes) => pump.encoded.extend(envelopes),
                Err(error) => pump.rejected.push((frame.id().to_owned(), error)),
            },
            Err(error) => pump.rejected.push((frame.id().to_owned(), error)),
        }
    }
    // The frames go out through the caller; the link's queue copy would only
    // fill toward its cap.
    link.take_outgoing();
    pump
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handler store, the declarations and the Java queues are
    /// process-wide, so tests that touch them run one at a time.
    fn global_state() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn declaration(
        id: &str,
        direction: PayloadDirection,
        codec: Option<&str>,
    ) -> PayloadDeclaration {
        PayloadDeclaration {
            id: id.into(),
            direction,
            codec: codec.map(str::to_owned),
        }
    }

    fn registry(declarations: impl IntoIterator<Item = PayloadDeclaration>) -> PayloadRegistry {
        let mut registry = PayloadRegistry::default();
        for declaration in declarations {
            registry.register(declaration).unwrap();
        }
        registry
    }

    fn link() -> PayloadLink {
        PayloadLink::new(registry([declaration(
            "demo:to_server",
            PayloadDirection::PlayToServer,
            Some("minecraft:string"),
        )]))
    }

    /// A client link over `demo:down` (to the client) and `demo:up` (to the
    /// server), both empty-codec so the frames validate.
    fn client_link() -> PayloadLink {
        PayloadLink::new(registry([
            declaration(
                "demo:down",
                PayloadDirection::PlayToClient,
                Some("minecraft:unit"),
            ),
            declaration(
                "demo:up",
                PayloadDirection::PlayToServer,
                Some("minecraft:unit"),
            ),
        ]))
    }

    fn empty_frame(id: &str, direction: PayloadDirection) -> PayloadFrame {
        PayloadFrame::new(id, direction, Vec::new()).unwrap()
    }

    // ---- declarations -------------------------------------------------

    #[test]
    fn registry_orders_and_rejects_conflicts() {
        let mut registry = PayloadRegistry::default();
        for row in [
            ("demo:beta", "playToServer", None),
            ("demo:alpha", "playToClient", Some("demo:codec")),
            ("ruby:gamma", "playBidirectional", None),
        ] {
            let direction = PayloadDirection::parse(row.1).unwrap();
            registry
                .register(declaration(row.0, direction, row.2))
                .unwrap();
        }
        let ids: Vec<_> = registry.declarations().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["demo:alpha", "demo:beta", "ruby:gamma"]);

        // The same declaration again is the same registration; any other
        // shape of a taken id is a conflict.
        let alpha = PayloadDirection::PlayToClient;
        assert!(
            registry
                .register(declaration("demo:alpha", alpha, Some("demo:codec")))
                .is_ok()
        );
        assert!(
            registry
                .register(declaration("demo:alpha", alpha, None))
                .is_err()
        );
        assert!(
            registry
                .register(declaration(
                    "demo:alpha",
                    PayloadDirection::PlayToServer,
                    None
                ))
                .is_err()
        );
        // Declarations are resource locations, codecs too.
        for bad in ["alpha", "Demo:alpha", "demo:", "demo:a b"] {
            assert!(
                registry.register(declaration(bad, alpha, None)).is_err(),
                "{bad}"
            );
        }
        assert!(
            registry
                .register(declaration("demo:bad", alpha, Some("not an id")))
                .is_err()
        );
    }

    #[test]
    fn registration_round_trips_and_replaces_the_registry() {
        let source = PayloadLink::new(registry([
            declaration(
                "demo:a",
                PayloadDirection::PlayToServer,
                Some("minecraft:raw"),
            ),
            declaration("demo:b", PayloadDirection::PlayToClient, None),
        ]));
        let mut target = PayloadLink::default();
        assert_eq!(
            target.apply_registration(&source.registration()).unwrap(),
            2
        );
        assert_eq!(target.registry().declarations().count(), 2);
        assert!(target.apply_registration(&[0xff]).is_err());
    }

    #[test]
    fn registration_cap_keeps_only_complete_declarations() {
        let mut source_registry = PayloadRegistry::default();
        for index in 0..4_096 {
            source_registry
                .register(declaration(
                    &format!("demo:channel_{index}"),
                    PayloadDirection::PlayToServer,
                    None,
                ))
                .unwrap();
        }
        let source = PayloadLink::new(source_registry);
        let encoded = source.registration();
        assert!(encoded.len() <= MAX_REGISTRATION);

        let mut target = PayloadLink::default();
        let applied = target.apply_registration(&encoded).unwrap();
        assert!(applied < 4_096);
        assert_eq!(target.registry().declarations().count(), applied);
    }

    // ---- direction filtering -------------------------------------------

    #[test]
    fn receive_validates_channel_direction_and_codec() {
        let mut link = link();
        let frame = PayloadFrame::new(
            "demo:to_server",
            PayloadDirection::PlayToServer,
            b"hello".to_vec(),
        )
        .unwrap();
        assert!(
            link.receive(&frame.encode(), PayloadDirection::PlayToServer)
                .is_ok()
        );
        assert_eq!(link.incoming().len(), 1);
        // Wrong direction, unknown channel, invalid UTF-8 and trailing bytes.
        assert!(
            link.receive(&frame.encode(), PayloadDirection::PlayToClient)
                .is_err()
        );
        let unknown =
            PayloadFrame::new("demo:other", PayloadDirection::PlayToServer, Vec::new()).unwrap();
        assert!(
            link.receive(&unknown.encode(), PayloadDirection::PlayToServer)
                .is_err()
        );
        let bad = PayloadFrame::new("demo:to_server", PayloadDirection::PlayToServer, vec![0xff])
            .unwrap();
        assert!(
            link.receive(&bad.encode(), PayloadDirection::PlayToServer)
                .is_err()
        );
        let mut trailing = frame.encode();
        trailing.push(0);
        assert!(
            link.receive(&trailing, PayloadDirection::PlayToServer)
                .is_err()
        );
    }

    #[test]
    fn send_queues_only_declared_valid_frames() {
        let mut link = link();
        let encoded = link
            .send(
                "demo:to_server",
                PayloadDirection::PlayToServer,
                b"ok".to_vec(),
            )
            .unwrap();
        assert_eq!(link.take_outgoing(), vec![encoded.clone()]);
        assert!(PayloadFrame::decode(&encoded).is_ok());
        assert!(
            link.send(
                "demo:to_server",
                PayloadDirection::PlayToClient,
                b"ok".to_vec()
            )
            .is_err()
        );
        assert!(
            link.send("demo:missing", PayloadDirection::PlayToServer, Vec::new())
                .is_err()
        );
        assert!(
            link.send("demo:to_server", PayloadDirection::PlayToServer, vec![0xff])
                .is_err()
        );
    }

    #[test]
    fn bidirectional_declarations_accept_only_play_directions() {
        let mut link = PayloadLink::new(registry([declaration(
            "demo:both",
            PayloadDirection::PlayBidirectional,
            Some("minecraft:empty"),
        )]));
        assert!(
            link.send("demo:both", PayloadDirection::PlayToServer, Vec::new())
                .is_ok()
        );
        assert!(
            link.send("demo:both", PayloadDirection::PlayToClient, Vec::new())
                .is_ok()
        );
        assert!(
            link.send(
                "demo:both",
                PayloadDirection::ConfigurationToClient,
                Vec::new()
            )
            .is_err()
        );
        let configuration_frame = PayloadFrame::new(
            "demo:both",
            PayloadDirection::ConfigurationToServer,
            Vec::new(),
        )
        .unwrap();
        assert!(
            link.receive(
                &configuration_frame.encode(),
                PayloadDirection::ConfigurationToServer
            )
            .is_err()
        );
        assert!(
            link.send("demo:both", PayloadDirection::PlayToServer, vec![1])
                .is_err()
        );
    }

    #[test]
    fn server_and_client_links_exchange_declared_frames() {
        let declarations = [
            declaration(
                "demo:up",
                PayloadDirection::PlayToServer,
                Some("minecraft:string"),
            ),
            declaration("demo:down", PayloadDirection::PlayToClient, None),
        ];
        let mut server = PayloadLink::new(registry(declarations.clone()));
        let mut client = PayloadLink::default();
        client.apply_registration(&server.registration()).unwrap();
        let up = client
            .send("demo:up", PayloadDirection::PlayToServer, b"hi".to_vec())
            .unwrap();
        let frame = server.receive(&up, PayloadDirection::PlayToServer).unwrap();
        assert_eq!(frame.bytes(), b"hi");
        let down = server
            .send("demo:down", PayloadDirection::PlayToClient, b"ok".to_vec())
            .unwrap();
        client
            .receive(&down, PayloadDirection::PlayToClient)
            .unwrap();
        assert_eq!(client.take_incoming().len(), 1);
        assert_eq!(server.take_incoming().len(), 1);
    }

    // ---- chunking ------------------------------------------------------

    #[test]
    fn frames_within_the_envelope_travel_unchanged() {
        let small = vec![0x5a; MAX_ENVELOPE];
        let envelopes = chunk_envelopes(&small).unwrap();
        assert_eq!(
            envelopes,
            vec![small.clone()],
            "a v1 frame is its own envelope"
        );
        assert!(!is_chunk_envelope(&envelopes[0]));
        assert_eq!(chunk_envelopes(&[1, 2, 3]).unwrap(), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn oversized_frames_split_and_join_losslessly() {
        for len in [
            MAX_ENVELOPE + 1,
            MAX_ENVELOPE * 2,
            MAX_JOINED,
            MAX_PAYLOAD_BYTES,
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let envelopes = chunk_envelopes(&data).unwrap();
            assert!(envelopes.len() > 1);
            assert!(
                envelopes
                    .iter()
                    .all(|envelope| envelope.len() <= MAX_ENVELOPE && is_chunk_envelope(envelope))
            );
            let mut assembler = ChunkAssembler::new();
            let mut joined = None;
            for envelope in &envelopes {
                if let Some(frame) = assembler.push(envelope).unwrap() {
                    joined = Some(frame);
                }
            }
            assert_eq!(joined.as_deref(), Some(data.as_slice()));
            assert!(assembler.is_idle());
        }
        // The payload model's cap still applies to a joined frame.
        assert!(chunk_envelopes(&vec![0; MAX_JOINED + 1]).is_err());
    }

    #[test]
    fn a_frame_beyond_the_envelope_round_trips_through_chunks() {
        let frame = PayloadFrame::new(
            "demo:big",
            PayloadDirection::PlayToServer,
            vec![7; MAX_ENVELOPE * 2],
        )
        .unwrap();
        let encoded = frame.encode();
        assert!(encoded.len() > MAX_ENVELOPE);
        let mut assembler = ChunkAssembler::new();
        let mut joined = Vec::new();
        for envelope in chunk_envelopes(&encoded).unwrap() {
            if let Some(data) = assembler.push(&envelope).unwrap() {
                joined = data;
            }
        }
        let (decoded, used) = PayloadFrame::decode(&joined).unwrap();
        assert_eq!(used, joined.len());
        assert_eq!(decoded, frame);
    }

    #[test]
    fn malformed_chunk_sequences_reject_and_reset() {
        let data = vec![1u8; MAX_ENVELOPE * 2];
        let envelopes = chunk_envelopes(&data).unwrap();

        // Chunks must arrive in order from index 0.
        let mut assembler = ChunkAssembler::new();
        assert!(assembler.push(&envelopes[1]).is_err());
        assert!(assembler.is_idle());

        // A dropped tail never completes; a fresh transfer replaces it.
        let mut assembler = ChunkAssembler::new();
        for envelope in &envelopes[..envelopes.len() - 1] {
            assert!(assembler.push(envelope).unwrap().is_none());
        }
        assert!(!assembler.is_idle());
        assert!(assembler.push(&envelopes[0]).unwrap().is_none());
        assert!(assembler.push(&envelopes[1]).unwrap().is_none());

        // A count or total past the bounds rejects before any allocation.
        let mut over_count = envelopes[0].clone();
        over_count[7..9].copy_from_slice(&(MAX_CHUNKS as u16 + 1).to_be_bytes());
        assert!(ChunkAssembler::new().push(&over_count).is_err());
        let mut over_total = envelopes[0].clone();
        over_total[9..13].copy_from_slice(&(MAX_JOINED as u32 + 1).to_be_bytes());
        assert!(ChunkAssembler::new().push(&over_total).is_err());
        // An index outside the declared count rejects.
        let mut over_index = envelopes[0].clone();
        over_index[5..7].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(ChunkAssembler::new().push(&over_index).is_err());
        // A whole frame is not a chunk envelope.
        let frame = PayloadFrame::new("demo:x", PayloadDirection::PlayToServer, vec![1]).unwrap();
        assert!(ChunkAssembler::new().push(&frame.encode()).is_err());
    }

    #[test]
    fn receive_payload_joins_chunks_and_refuses_malformed_sequences() {
        let mut link = PayloadLink::new(registry([declaration(
            "demo:down",
            PayloadDirection::PlayToClient,
            Some("minecraft:raw"),
        )]));
        let frame = PayloadFrame::new(
            "demo:down",
            PayloadDirection::PlayToClient,
            vec![9; MAX_ENVELOPE * 2],
        )
        .unwrap();
        let envelopes = chunk_envelopes(&frame.encode()).unwrap();
        let mut chunks = ChunkAssembler::new();
        for envelope in &envelopes[..envelopes.len() - 1] {
            assert!(!receive_payload(&mut link, &mut chunks, envelope).unwrap());
        }
        assert!(receive_payload(&mut link, &mut chunks, envelopes.last().unwrap()).unwrap());
        assert_eq!(link.take_incoming(), vec![frame]);
        assert!(chunks.is_idle());

        // An out-of-order chunk rejects and resets the assembler.
        let mut chunks = ChunkAssembler::new();
        assert!(!receive_payload(&mut link, &mut chunks, &envelopes[0]).unwrap());
        assert!(
            receive_payload(&mut link, &mut chunks, &envelopes[2]).is_err(),
            "chunk 2 arrived where chunk 1 was due"
        );
        assert!(chunks.is_idle());

        // A whole frame mid-transfer takes the stale transfer down with it.
        let mut chunks = ChunkAssembler::new();
        assert!(!receive_payload(&mut link, &mut chunks, &envelopes[0]).unwrap());
        let whole = empty_frame("demo:down", PayloadDirection::PlayToClient).encode();
        assert!(receive_payload(&mut link, &mut chunks, &whole).is_err());
        assert!(chunks.is_idle());
        assert!(link.incoming().is_empty());
    }

    #[test]
    fn registration_chunks_join_and_malformed_sequences_reset() {
        let mut source_registry = PayloadRegistry::default();
        for n in 0..1_100 {
            source_registry
                .register(declaration(
                    &format!("demo:channel_{n:04}_with_a_long_name"),
                    PayloadDirection::PlayToClient,
                    None,
                ))
                .unwrap();
        }
        let source = PayloadLink::new(source_registry);
        let encoded = source.registration();
        assert!(encoded.len() > 40 * 1024);
        let envelopes = chunk_envelopes(&encoded).unwrap();
        let mut target = PayloadLink::default();
        let mut chunks = ChunkAssembler::new();
        for envelope in &envelopes[..envelopes.len() - 1] {
            assert_eq!(
                receive_registration(&mut target, &mut chunks, envelope).unwrap(),
                None
            );
        }
        assert!(!chunks.is_idle());
        let count = receive_registration(&mut target, &mut chunks, envelopes.last().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(count, 1_100);
        assert_eq!(target.registry().declarations().count(), 1_100);

        let mut malformed = envelopes[0].clone();
        malformed[7..9].copy_from_slice(&(MAX_CHUNKS as u16 + 1).to_be_bytes());
        assert!(receive_registration(&mut target, &mut chunks, &malformed).is_err());
        assert!(chunks.is_idle());
    }

    // ---- handlers and queues -------------------------------------------

    #[test]
    fn java_host_frames_publish_through_the_channel_registry() {
        let registry = registry([
            declaration(
                "demo:up",
                PayloadDirection::PlayToServer,
                Some("minecraft:string"),
            ),
            declaration("demo:down", PayloadDirection::PlayToClient, None),
        ]);
        let mut host = StdHost::new();
        host.queue_outgoing_payload("demo:up", "playToServer", b"hi".to_vec());
        host.queue_outgoing_payload("demo:down", "playToClient", b"ok".to_vec());
        let frames = publish_from_host_with(&registry, &mut host).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].id(), "demo:up");
        assert_eq!(frames[0].bytes(), b"hi");
        assert!(host.take_outgoing_payloads().is_empty());
        // Undeclared, wrong direction and bad codec all stop the batch.
        let mut bad = StdHost::new();
        bad.queue_outgoing_payload("demo:missing", "playToServer", Vec::new());
        assert!(publish_from_host_with(&registry, &mut bad).is_err());
        let mut wrong = StdHost::new();
        wrong.queue_outgoing_payload("demo:up", "playToClient", Vec::new());
        assert!(publish_from_host_with(&registry, &mut wrong).is_err());
        let mut codec = StdHost::new();
        codec.queue_outgoing_payload("demo:up", "playToServer", vec![0xff]);
        assert!(publish_from_host_with(&registry, &mut codec).is_err());
    }

    #[test]
    fn a_networking_send_queues_the_payload_marker_and_bytes() {
        use horizon_native_java::{Host, MethodRef};
        let mut host = StdHost::new()
            .with_packet_type("payload#1", "demo:up")
            .with_payload_bytes("payload#1", b"wire".to_vec());
        let send = MethodRef {
            owner: "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking".into(),
            name: "send".into(),
            descriptor: "(Ljava/lang/Object;Lnet/minecraft/network/protocol/common/custom/CustomPacketPayload;)V".into(),
        };
        Host::invoke(&mut host, &send, &[Value::Ref(Some("payload#1".into()))]).unwrap();
        let queued = host.take_outgoing_payloads();
        assert_eq!(
            queued,
            vec![(
                "demo:up".to_owned(),
                "playToClient".to_owned(),
                b"wire".to_vec()
            )]
        );
    }

    #[test]
    fn received_frames_reach_the_java_host() {
        let mut host = StdHost::new();
        let frame = PayloadFrame::new("demo:up", PayloadDirection::PlayToServer, b"hello".to_vec())
            .unwrap();
        assert_eq!(deliver_to_host(&mut host, [frame]), 1);
        assert_eq!(
            host.take_incoming_payloads(),
            vec![("demo:up".to_owned(), b"hello".to_vec())]
        );
    }

    /// A frame no stored jar registered for is consumed and reported, not
    /// retried: the pump records it once and the queue stays empty.
    #[test]
    fn without_a_handler_a_received_frame_is_consumed_and_reported() {
        let _guard = global_state();
        clear_handler_jars();
        let mut link = PayloadLink::new(registry([declaration(
            "demo:up",
            PayloadDirection::PlayToClient,
            None,
        )]));
        for _ in 0..3 {
            let data = empty_frame("demo:up", PayloadDirection::PlayToClient).encode();
            link.receive(&data, PayloadDirection::PlayToClient).unwrap();
        }
        let pump = pump_client_payloads(&mut link, 2, 0, &mut |_| Ok(false), &mut |_| Vec::new());
        assert_eq!(pump.unhandled.len(), 2, "the drain is bounded");
        assert_eq!(pump.dispatched, 0);
        assert_eq!(link.incoming().len(), 1, "the remainder stays queued");
        let pump = pump_client_payloads(&mut link, 2, 0, &mut |_| Ok(false), &mut |_| Vec::new());
        assert_eq!(pump.unhandled.len(), 1);
        assert!(link.incoming().is_empty());
    }

    #[test]
    fn supported_codecs_decode_the_payload_value() {
        let string = declaration(
            "demo:text",
            PayloadDirection::PlayToServer,
            Some("minecraft:string"),
        );
        let frame = PayloadFrame::new(
            "demo:text",
            PayloadDirection::PlayToServer,
            b"hello".to_vec(),
        )
        .unwrap();
        assert_eq!(
            payload_values(Some(&string), &frame).0,
            Value::Ref(Some("hello".into()))
        );
        let empty = declaration(
            "demo:empty",
            PayloadDirection::PlayToServer,
            Some("minecraft:unit"),
        );
        let frame =
            PayloadFrame::new("demo:empty", PayloadDirection::PlayToServer, Vec::new()).unwrap();
        assert_eq!(
            payload_values(Some(&empty), &frame).0,
            Value::Ref(Some("payload-empty/demo:empty".into()))
        );
        let raw = declaration(
            "demo:raw",
            PayloadDirection::PlayToServer,
            Some("minecraft:byte_array"),
        );
        let frame =
            PayloadFrame::new("demo:raw", PayloadDirection::PlayToServer, vec![0xff]).unwrap();
        assert_eq!(
            payload_values(Some(&raw), &frame).0,
            Value::Ref(Some("payload-bytes/ff".into()))
        );
    }

    #[test]
    fn dispatch_distinguishes_raw_text_empty_and_markers() {
        let frame = |id: &str, bytes: Vec<u8>| {
            PayloadFrame::new(id, PayloadDirection::PlayToServer, bytes).unwrap()
        };
        // Raw bytes cross as their bounded hex, not the id-carrying marker.
        let raw = declaration(
            "demo:raw",
            PayloadDirection::PlayToServer,
            Some("minecraft:byte_array"),
        );
        assert_eq!(
            payload_values(Some(&raw), &frame("demo:raw", vec![0x00, 0xff])).0,
            Value::Ref(Some("payload-bytes/00ff".into()))
        );
        // An unmodelled codec and an undeclared channel keep the old marker.
        let custom = declaration(
            "demo:custom",
            PayloadDirection::PlayToServer,
            Some("demo:codec"),
        );
        assert_eq!(
            payload_values(Some(&custom), &frame("demo:custom", b"A".to_vec())).0,
            Value::Ref(Some("payload/demo:custom/41".into()))
        );
        assert_eq!(
            payload_values(None, &frame("demo:none", b"A".to_vec())).0,
            Value::Ref(Some("payload/demo:none/41".into()))
        );
        // Invalid UTF-8 falls back to the marker instead of a lossy string.
        let text = declaration(
            "demo:text",
            PayloadDirection::PlayToServer,
            Some("minecraft:string"),
        );
        assert_eq!(
            payload_values(Some(&text), &frame("demo:text", vec![0xff])).0,
            Value::Ref(Some("payload/demo:text/ff".into()))
        );
        // A large raw payload is bounded: 256 bytes of hex, not the whole frame.
        let marker = payload_values(Some(&raw), &frame("demo:raw", vec![0xab; 4_096])).0;
        assert_eq!(
            marker,
            Value::Ref(Some(format!("payload-bytes/{}", "ab".repeat(256))))
        );
    }

    #[test]
    fn take_incoming_up_to_keeps_the_remainder_queued() {
        let mut server = PayloadLink::new(registry([declaration(
            "demo:to_client",
            PayloadDirection::PlayToClient,
            None,
        )]));
        let mut client = server.clone();
        for n in 0..5u8 {
            let data = server
                .send("demo:to_client", PayloadDirection::PlayToClient, vec![n])
                .unwrap();
            client
                .receive(&data, PayloadDirection::PlayToClient)
                .unwrap();
        }
        let first = client.take_incoming_up_to(3);
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].bytes(), &[0]);
        assert_eq!(client.incoming().len(), 2);
        // Oldest first, and the rest comes out exactly once.
        let rest = client.take_incoming_up_to(100);
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].bytes(), &[3]);
        assert!(client.take_incoming_up_to(100).is_empty());
    }

    #[test]
    fn host_frames_for_the_server_drain_exactly_once_and_stay_bounded() {
        let _guard = global_state();
        clear_handler_jars();
        store_declared([
            declaration("demo:up", PayloadDirection::PlayToServer, None),
            declaration("demo:down", PayloadDirection::PlayToClient, None),
        ])
        .unwrap();
        let mut host = StdHost::new();
        host.queue_outgoing_payload("demo:up", "playToServer", vec![1]);
        host.queue_outgoing_payload("demo:down", "playToClient", vec![2]);
        host.queue_outgoing_payload("demo:up", "playToServer", vec![3]);
        assert_eq!(publish_from_host(&mut host).unwrap(), 3);

        // The client drain sees only what travels to the server, in order,
        // and a bounded call leaves the remainder queued.
        let first = take_java_to_server(1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].bytes(), &[1]);
        let rest = take_java_to_server(256);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].bytes(), &[3]);
        assert!(take_java_to_server(256).is_empty());

        // The shared queue the hub drains is untouched by the client drain.
        let shared = take_java_outgoing();
        assert_eq!(shared.len(), 3);
        assert!(take_java_outgoing().is_empty());

        // Nothing drains the mirror in a process without a client window:
        // the oldest frames are dropped rather than sends failing.
        for round in 0..3u8 {
            let mut host = StdHost::new();
            for n in 0..200u8 {
                host.queue_outgoing_payload("demo:up", "playToServer", vec![round, n]);
            }
            publish_from_host(&mut host).unwrap();
            take_java_outgoing();
        }
        let kept = take_java_to_server(usize::MAX);
        assert_eq!(kept.len(), MAX_OUTGOING);
        assert_eq!(kept.last().unwrap().bytes(), &[2, 199]);

        // A new content root discards frames queued under the old one.
        let mut host = StdHost::new();
        host.queue_outgoing_payload("demo:up", "playToServer", vec![9]);
        publish_from_host(&mut host).unwrap();
        store_declared([declaration("demo:up", PayloadDirection::PlayToServer, None)]).unwrap();
        assert!(take_java_to_server(256).is_empty());
    }

    /// The hub's drain takes only the frames a client can receive; the
    /// server-bound ones stay reachable in the client window's mirror.
    #[test]
    fn client_drain_takes_only_client_bound_frames_and_keeps_server_bound_ones() {
        let _guard = global_state();
        clear_handler_jars();
        store_declared([
            declaration("demo:up", PayloadDirection::PlayToServer, None),
            declaration("demo:down", PayloadDirection::PlayToClient, None),
            declaration("demo:both", PayloadDirection::PlayBidirectional, None),
        ])
        .unwrap();
        let mut host = StdHost::new();
        host.queue_outgoing_payload("demo:up", "playToServer", vec![1]);
        host.queue_outgoing_payload("demo:down", "playToClient", vec![2]);
        host.queue_outgoing_payload("demo:both", "playToServer", vec![3]);
        host.queue_outgoing_payload("demo:down", "playToClient", vec![4]);
        assert_eq!(publish_from_host(&mut host).unwrap(), 4);

        // A bounded call leaves the rest for the next tick.
        let first = take_java_to_clients(1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id(), "demo:down");
        assert_eq!(first[0].bytes(), &[2]);
        let rest = take_java_to_clients(256);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].bytes(), &[4]);
        assert!(take_java_to_clients(256).is_empty());

        // The server-bound frames were kept for the client window.
        let to_server = take_java_to_server(256);
        assert_eq!(to_server.len(), 2);
        assert_eq!(to_server[0].bytes(), &[1]);
        assert_eq!(to_server[1].bytes(), &[3]);
        assert!(take_java_outgoing().is_empty());
    }

    /// A new content root re-sends its declarations: every guest's flag
    /// clears, and its link accepts frames only for the new channels.
    #[test]
    fn a_new_content_root_sends_its_declarations_again() {
        let _guard = global_state();
        clear_handler_jars();
        store_declared([declaration(
            "demo:old",
            PayloadDirection::PlayToServer,
            None,
        )])
        .unwrap();
        {
            let mut links = GUEST_LINKS.lock().unwrap();
            links.insert(7, GuestLink::new());
            links.get_mut(&7).unwrap().registration_sent = true;
        }
        store_declared([declaration(
            "demo:new",
            PayloadDirection::PlayToServer,
            None,
        )])
        .unwrap();
        let mut links = GUEST_LINKS.lock().unwrap();
        let guest = links.get_mut(&7).unwrap();
        assert!(!guest.registration_sent, "the guest is sent the new list");
        let old = empty_frame("demo:old", PayloadDirection::PlayToServer).encode();
        assert!(
            guest.receive(&old).is_err(),
            "the replaced channel is no longer declared"
        );
        links.clear();
    }

    // ---- one client tick -----------------------------------------------

    fn receive_down(link: &mut PayloadLink, count: usize) {
        let data = empty_frame("demo:down", PayloadDirection::PlayToClient).encode();
        for _ in 0..count {
            link.receive(&data, PayloadDirection::PlayToClient).unwrap();
        }
    }

    /// The tick drain is bounded in both directions: a cap's worth of received
    /// frames is dispatched and sent, and the remainder stays queued.
    #[test]
    fn the_client_payload_drain_is_bounded_per_tick() {
        let mut link = client_link();
        receive_down(&mut link, 200);
        let mut dispatched = 0;
        let mut pending: Vec<PayloadFrame> = (0..30)
            .map(|_| empty_frame("demo:up", PayloadDirection::PlayToServer))
            .collect();
        let pump = pump_client_payloads(
            &mut link,
            128,
            16,
            &mut |_| {
                dispatched += 1;
                Ok(true)
            },
            &mut |max| pending.drain(..max.min(pending.len())).collect(),
        );
        assert_eq!(dispatched, 128);
        assert_eq!(link.incoming().len(), 72);
        assert_eq!(pump.encoded.len(), 16, "each small frame is one body");
        assert!(pump.rejected.is_empty());
        assert_eq!(pending.len(), 14);

        // A tick with no sends still drains received frames, and the link's
        // own queue copy never fills up.
        let pump = pump_client_payloads(&mut link, 256, 0, &mut |_| Ok(true), &mut |_| Vec::new());
        assert_eq!(pump.dispatched, 72);
        assert!(link.incoming().is_empty());
        assert!(link.outgoing().is_empty());
    }

    /// A frame far above the packet envelope is a valid payload: the client
    /// sends it as consecutive chunk envelopes the server joins back.
    #[test]
    fn a_large_client_send_leaves_as_chunks_the_server_joins() {
        let mut link = PayloadLink::new(registry([declaration(
            "demo:up",
            PayloadDirection::PlayToServer,
            Some("minecraft:raw"),
        )]));
        let big = PayloadFrame::new(
            "demo:up",
            PayloadDirection::PlayToServer,
            vec![0xab; MAX_ENVELOPE * 2],
        )
        .unwrap();
        let mut pending = vec![big.clone()];
        let pump = pump_client_payloads(&mut link, 0, 256, &mut |_| Ok(true), &mut |max| {
            pending.drain(..max.min(pending.len())).collect()
        });
        assert!(pump.encoded.len() > 1);
        assert!(
            pump.encoded
                .iter()
                .all(|envelope| envelope.len() <= MAX_ENVELOPE && is_chunk_envelope(envelope))
        );

        // The server's assembler joins them into the frame that was sent.
        let mut server_chunks = ChunkAssembler::new();
        let mut joined = None;
        for envelope in &pump.encoded {
            if let Some(data) = server_chunks.push(envelope).unwrap() {
                joined = Some(data);
            }
        }
        let (decoded, used) = PayloadFrame::decode(&joined.unwrap()).unwrap();
        assert_eq!(used, big.encode().len());
        assert_eq!(decoded, big);

        // A frame that is not server-bound is refused before it is encoded.
        let mut link = client_link();
        let wrong = empty_frame("demo:down", PayloadDirection::PlayToClient);
        let mut pending = vec![wrong];
        let pump = pump_client_payloads(&mut link, 0, 1, &mut |_| Ok(true), &mut |max| {
            pending.drain(..max.min(pending.len())).collect()
        });
        assert!(pump.encoded.is_empty());
        assert_eq!(pump.rejected.len(), 1);
        assert_eq!(pump.rejected[0].0, "demo:down");
    }

    /// A frame that fits the envelope is its own body, unchanged on the wire.
    #[test]
    fn a_small_client_send_is_unchanged() {
        let mut link = client_link();
        let mut pending = vec![empty_frame("demo:up", PayloadDirection::PlayToServer)];
        let pump = pump_client_payloads(&mut link, 0, 1, &mut |_| Ok(true), &mut |max| {
            pending.drain(..max.min(pending.len())).collect()
        });
        assert_eq!(pump.encoded.len(), 1);
        assert!(!is_chunk_envelope(&pump.encoded[0]));
        let (decoded, used) = PayloadFrame::decode(&pump.encoded[0]).unwrap();
        assert_eq!(used, pump.encoded[0].len());
        assert_eq!(decoded.id(), "demo:up");
        assert_eq!(decoded.direction(), PayloadDirection::PlayToServer);
    }

    /// A send before the connection has joined waits: the caller passes a zero
    /// send cap, and the frames stay in the queue for a later tick.
    #[test]
    fn sends_wait_until_the_connection_has_joined() {
        let mut link = client_link();
        let mut pending = vec![empty_frame("demo:up", PayloadDirection::PlayToServer)];
        let pump = pump_client_payloads(&mut link, 256, 0, &mut |_| Ok(true), &mut |max| {
            pending.drain(..max.min(pending.len())).collect()
        });
        assert!(pump.encoded.is_empty());
        assert_eq!(pending.len(), 1, "the send is not consumed");
    }

    /// The channels a host announced at bootstrap, and the host itself, become
    /// the transport's declarations and its handler.
    #[test]
    fn adopt_host_registers_the_channels_it_announced() {
        use horizon_native_java::{Host, MethodRef};
        let _guard = global_state();
        clear_handler_jars();
        let mut host = StdHost::new()
            .with_packet_type("payload#1", "demo:frame")
            .with_packet_type("payload#2", "demo:reply");
        let register = MethodRef {
            owner: "net/fabricmc/fabric/api/client/networking/v1/ClientPlayNetworking".into(),
            name: "registerGlobalReceiver".into(),
            descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Z".into(),
        };
        for marker in ["payload#1", "payload#2"] {
            Host::invoke(
                &mut host,
                &register,
                &[
                    Value::Ref(Some(marker.into())),
                    Value::Ref(Some(format!("handler#{marker}"))),
                ],
            )
            .unwrap();
        }
        assert_eq!(host.payload_registrations().len(), 2);
        let jar = NativeJar::default();
        let mut declarations = Vec::new();
        assert_eq!(
            adopt_host(HandlerRole::Client, &jar, &mut host, &mut declarations),
            2
        );

        let declared = declared_by_id();
        assert_eq!(
            declared["demo:frame"].direction,
            PayloadDirection::PlayToClient
        );
        assert_eq!(
            declared["demo:reply"].direction,
            PayloadDirection::PlayToClient
        );
        // A second jar's channels join the first's instead of replacing them.
        let mut other = StdHost::new().with_packet_type("payload#3", "demo:other");
        Host::invoke(
            &mut other,
            &register,
            &[
                Value::Ref(Some("payload#3".into())),
                Value::Ref(Some("handler#3".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            adopt_host(HandlerRole::Client, &jar, &mut other, &mut declarations),
            3
        );
        assert_eq!(declared_by_id().len(), 3);

        // The stored host answers for these channels: the frame resolves to a
        // registration instead of "nothing registered".
        let frame = empty_frame("demo:frame", PayloadDirection::PlayToClient);
        assert_eq!(
            dispatch_received_as(&frame, PayloadDirection::PlayToClient),
            Ok(false),
            "a handler marker this bounded path cannot call claims nothing"
        );
        clear_handler_jars();
    }
}
