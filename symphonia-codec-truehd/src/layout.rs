// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::Position;

/// The maximum number of channels a layout may contain (all 13 TrueHD channel groups).
const MAX_LAYOUT_CHANNELS: usize = 20;

/// An ordered list of channel positions in the order channels are *assigned* by the bitstream
/// (the Dolby channel order for TrueHD, or the usual channel bit order for MLP).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelList {
    len: usize,
    pos: [Position; MAX_LAYOUT_CHANNELS],
}

impl ChannelList {
    pub(crate) const EMPTY: ChannelList =
        ChannelList { len: 0, pos: [Position::empty(); MAX_LAYOUT_CHANNELS] };

    fn push(&mut self, pos: Position) {
        self.pos[self.len] = pos;
        self.len += 1;
    }

    /// The number of channels.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The set of positions in the list.
    pub(crate) fn mask(&self) -> Position {
        self.pos[..self.len].iter().fold(Position::empty(), |m, &p| m | p)
    }

    /// The position of the `idx`-th channel in assignment order.
    pub(crate) fn get(&self, idx: usize) -> Option<Position> {
        self.pos[..self.len].get(idx).copied()
    }

    /// The index of `pos` within the sorted (canonical audio buffer) channel order.
    pub(crate) fn canonical_index(&self, pos: Position) -> usize {
        (self.mask().bits() & (pos.bits() - 1)).count_ones() as usize
    }

    /// The layout of a mono signal.
    pub(crate) fn mono() -> ChannelList {
        let mut l = ChannelList::EMPTY;
        l.push(Position::FRONT_CENTER);
        l
    }

    /// The layout of a stereo signal.
    pub(crate) fn stereo() -> ChannelList {
        let mut l = ChannelList::EMPTY;
        l.push(Position::FRONT_LEFT);
        l.push(Position::FRONT_RIGHT);
        l
    }

    /// The layout for a TrueHD channel assignment bitmap (13 bits).
    pub(crate) fn from_truehd_map(chanmap: u32) -> ChannelList {
        // The channel groups in bit order of the channel assignment.
        const GROUPS: [&[Position]; 13] = [
            // LR
            &[Position::FRONT_LEFT, Position::FRONT_RIGHT],
            // C
            &[Position::FRONT_CENTER],
            // LFE
            &[Position::LFE1],
            // LRs
            &[Position::SIDE_LEFT, Position::SIDE_RIGHT],
            // LRvh
            &[Position::TOP_FRONT_LEFT, Position::TOP_FRONT_RIGHT],
            // LRc
            &[Position::FRONT_LEFT_CENTER, Position::FRONT_RIGHT_CENTER],
            // LRrs
            &[Position::REAR_LEFT, Position::REAR_RIGHT],
            // Cs
            &[Position::REAR_CENTER],
            // Ts
            &[Position::TOP_CENTER],
            // LRsd (surround direct)
            &[Position::SURROUND_DIRECT_LEFT, Position::SURROUND_DIRECT_RIGHT],
            // LRw
            &[Position::FRONT_LEFT_WIDE, Position::FRONT_RIGHT_WIDE],
            // Cvh
            &[Position::TOP_FRONT_CENTER],
            // LFE2
            &[Position::LFE2],
        ];

        let mut list = ChannelList::EMPTY;

        for (bit, group) in GROUPS.iter().enumerate() {
            if chanmap & (1 << bit) != 0 {
                for &pos in *group {
                    list.push(pos);
                }
            }
        }

        list
    }

    /// The layout for an MLP channel arrangement (0..=20). Returns `None` for an unassigned
    /// arrangement.
    pub(crate) fn from_mlp_arrangement(arrangement: u32) -> Option<ChannelList> {
        const FL: Position = Position::FRONT_LEFT;
        const FR: Position = Position::FRONT_RIGHT;
        const FC: Position = Position::FRONT_CENTER;
        const LFE: Position = Position::LFE1;
        const BL: Position = Position::REAR_LEFT;
        const BR: Position = Position::REAR_RIGHT;
        const BC: Position = Position::REAR_CENTER;

        let mask = match arrangement {
            0 => FC,
            1 => FL | FR,
            2 => FL | FR | BC,
            3 => FL | FR | BL | BR,
            4 => FL | FR | LFE,
            5 => FL | FR | LFE | BC,
            6 => FL | FR | LFE | BL | BR,
            7 => FL | FR | FC,
            8 => FL | FR | FC | BC,
            9 => FL | FR | FC | BL | BR,
            10 => FL | FR | FC | LFE,
            11 => FL | FR | FC | LFE | BC,
            12 => FL | FR | FC | LFE | BL | BR,
            13 => FL | FR | FC | BC,
            14 => FL | FR | FC | BL | BR,
            15 => FL | FR | FC | LFE,
            16 => FL | FR | FC | LFE | BC,
            17 => FL | FR | FC | LFE | BL | BR,
            18 => FL | FR | LFE | BL | BR,
            19 => FL | FR | FC | BL | BR,
            20 => FL | FR | FC | LFE | BL | BR,
            _ => return None,
        };

        // MLP channels are assigned in canonical channel order.
        let mut list = ChannelList::EMPTY;
        for pos in mask.iter() {
            list.push(pos);
        }
        Some(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_surround_direct_is_distinct_from_side() {
        // LRs (bit 3) and LRsd (bit 9) together.
        let list = ChannelList::from_truehd_map((1 << 3) | (1 << 9));
        assert_eq!(list.len(), 4);
        assert_eq!(list.get(0), Some(Position::SIDE_LEFT));
        assert_eq!(list.get(2), Some(Position::SURROUND_DIRECT_LEFT));
        assert_eq!(list.mask().bits().count_ones(), 4);
    }
}
