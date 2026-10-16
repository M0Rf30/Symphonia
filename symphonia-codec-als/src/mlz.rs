// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Masked LZ decompression of the floating-point part of ALS (ISO/IEC 14496-3 subpart 11,
//! "RAFP"), a port of FFmpeg `libavcodec/mlz.c` (LGPL-2.1+, Copyright (c) 2014 Ivan Kalvachev
//! and Thilo Borgmann), see the `NOTICE` file.

use crate::bits::Bits;

const CODE_UNSET: i32 = -1;
const CODE_BIT_INIT: u32 = 9;
const DIC_INDEX_INIT: i32 = 512;
const DIC_INDEX_MAX: i32 = 32768;
const FLUSH_CODE: i32 = 256;
const FREEZE_CODE: i32 = 257;
const FIRST_CODE: i32 = 258;
const MAX_CODE: i32 = 32767;
/// Must be a prime number.
const TABLE_SIZE: usize = 35023;

#[derive(Clone, Copy)]
struct Entry {
    parent_code: i32,
    char_code: i32,
    match_len: i32,
}

const UNSET_ENTRY: Entry = Entry { parent_code: CODE_UNSET, char_code: 0, match_len: 0 };

pub struct Mlz {
    dict: Vec<Entry>,
    dic_code_bit: u32,
    current_dic_index_max: i32,
    bump_code: i32,
    next_code: i32,
    freeze_flag: bool,
}

impl Mlz {
    pub fn new() -> Self {
        Mlz {
            dict: vec![UNSET_ENTRY; TABLE_SIZE],
            dic_code_bit: CODE_BIT_INIT,
            current_dic_index_max: DIC_INDEX_INIT,
            bump_code: DIC_INDEX_INIT - 1,
            next_code: FIRST_CODE,
            freeze_flag: false,
        }
    }

    pub fn flush_dict(&mut self) {
        self.dict.fill(UNSET_ENTRY);
        self.current_dic_index_max = DIC_INDEX_INIT;
        self.dic_code_bit = CODE_BIT_INIT;
        self.bump_code = self.current_dic_index_max - 1;
        self.next_code = FIRST_CODE;
        self.freeze_flag = false;
    }

    fn set_new_entry(&mut self, string_code: i32, parent_code: i32, char_code: i32) {
        let match_len = if parent_code < FIRST_CODE {
            2
        }
        else {
            self.dict.get(parent_code as usize).map_or(0, |e| e.match_len) + 1
        };

        if let Some(e) = self.dict.get_mut(string_code as usize) {
            *e = Entry { parent_code, char_code, match_len };
        }
    }

    /// Expand `string_code` into `buff` (FFmpeg `decode_string`). Returns the number of bytes
    /// written. `first_char_code` receives the first character of the string.
    fn decode_string(&self, buff: &mut [u8], string_code: i32, first_char_code: &mut i32) -> usize {
        let bufsize = buff.len();
        let mut count = 0usize;
        let mut current_code = string_code;
        *first_char_code = CODE_UNSET;

        while count < bufsize {
            if current_code == CODE_UNSET {
                return count;
            }

            if current_code < FIRST_CODE {
                *first_char_code = current_code;
                buff[0] = current_code as u8;
                return count + 1;
            }

            let Some(entry) = self.dict.get(current_code as usize)
            else {
                return count;
            };

            let offset = (entry.match_len - 1) as usize;
            if offset >= bufsize {
                return count;
            }
            buff[offset] = entry.char_code as u8;
            count += 1;

            current_code = entry.parent_code;
            if !(0..DIC_INDEX_MAX).contains(&current_code) {
                return count;
            }

            if current_code > FIRST_CODE {
                let entry = &self.dict[current_code as usize];
                if !(0..DIC_INDEX_MAX).contains(&entry.parent_code) {
                    return count;
                }
                // The offset is unsigned in the reference, so a negative one is out of range too.
                if !(1..=DIC_INDEX_MAX).contains(&entry.match_len) {
                    return count;
                }
            }
        }

        count
    }

    /// Decompress until `size` bytes are produced into `buff` (FFmpeg `ff_mlz_decompression`).
    /// Returns the number of bytes produced, which is less than `size` on error.
    pub fn decompress(&mut self, bits: &mut Bits<'_>, size: usize, buff: &mut [u8]) -> usize {
        let size = size.min(buff.len());
        let mut output_chars = 0usize;
        let mut char_code = -1;
        let mut last_string_code = -1;

        while output_chars < size {
            // A corrupt stream must not make this spin on the zero bits past the end.
            if bits.left() < -64 || self.dic_code_bit > 24 {
                return output_chars;
            }

            let mut string_code = 0i32;
            for i in 0..self.dic_code_bit {
                string_code |= (bits.get1() as i32) << i;
            }

            match string_code {
                FLUSH_CODE | MAX_CODE => {
                    self.flush_dict();
                    char_code = -1;
                    last_string_code = -1;
                }
                FREEZE_CODE => self.freeze_flag = true,
                _ => {
                    if string_code > self.current_dic_index_max {
                        return output_chars;
                    }

                    if string_code == self.bump_code {
                        self.dic_code_bit += 1;
                        self.current_dic_index_max *= 2;
                        self.bump_code = self.current_dic_index_max - 1;
                        continue;
                    }

                    if string_code >= self.next_code {
                        let n = self.decode_string(
                            &mut buff[output_chars..size],
                            last_string_code,
                            &mut char_code,
                        );
                        output_chars += n;

                        let n = self.decode_string(
                            &mut buff[output_chars..size],
                            char_code,
                            &mut char_code,
                        );
                        output_chars += n;

                        self.set_new_entry(self.next_code, last_string_code, char_code);
                        if self.next_code as usize >= TABLE_SIZE - 1 {
                            return output_chars;
                        }
                        self.next_code += 1;
                    }
                    else {
                        let n = self.decode_string(
                            &mut buff[output_chars..size],
                            string_code,
                            &mut char_code,
                        );
                        output_chars += n;

                        if !self.freeze_flag {
                            if last_string_code != -1 {
                                self.set_new_entry(self.next_code, last_string_code, char_code);
                                if self.next_code as usize >= TABLE_SIZE - 1 {
                                    return output_chars;
                                }
                                self.next_code += 1;
                            }
                        }
                        else {
                            // The reference breaks out of the switch without updating
                            // `last_string_code`.
                            continue;
                        }
                    }

                    last_string_code = string_code;
                }
            }
        }

        output_chars
    }
}
