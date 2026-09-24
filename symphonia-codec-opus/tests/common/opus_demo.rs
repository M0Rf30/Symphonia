// Parser for the RFC 8251 test vector `.bit` format, produced by libopus's `opus_demo`.
//
// Layout, repeated per packet:
//   u32 BE payload length (0 == lost packet)
//   u32 BE encoder's final range (`OPUS_GET_FINAL_RANGE`) at the time this packet was encoded
//   `length` bytes of Opus payload (absent when length == 0)

pub struct BitFilePacket {
    pub payload: Vec<u8>,
    pub enc_final_range: u32,
    pub lost: bool,
}

pub struct BitFile {
    packets: Vec<BitFilePacket>,
}

impl BitFile {
    pub fn parse(data: &[u8]) -> Self {
        let mut packets = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let len = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
            let range = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
            pos += 8;
            let lost = len == 0;
            let payload = if lost {
                Vec::new()
            }
            else {
                let end = pos + len as usize;
                assert!(end <= data.len(), "truncated .bit file");
                let p = data[pos..end].to_vec();
                pos = end;
                p
            };
            packets.push(BitFilePacket { payload, enc_final_range: range, lost });
        }
        BitFile { packets }
    }

    pub fn iter(&self) -> impl Iterator<Item = &BitFilePacket> {
        self.packets.iter()
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}
