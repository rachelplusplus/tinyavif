// Copyright (c) 2024-2025, The tinyavif contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

use crate::util::*;

pub struct EntropyWriter {
  // We need to be able to modify already-written bytes for carry propagation,
  // so we have to write into a Vec<u8> rather than a generic Write instance
  data: Vec<u8>,

  low: u64,
  range: u32,
  count: i32
}

impl EntropyWriter {
  pub fn new() -> Self {
    Self {
      data: Vec::new(),
      low: 0u64,
      range: 0x8000u32,
      count: -9i32
    }
  }

  // Sometimes we need to propagate a carry into the existing bytes
  // This function handles the core loop of that operation
  // Note: This assumes that the incoming carry is always 1, as it should
  // never be any other value.
  fn propagate_carry(&mut self) {
    for i in (0..self.data.len()).rev() {
      if self.data[i] == 255 {
        // Carry continues to propagate
        self.data[i] = 0;
        continue;
      } else {
        // Carry stops here
        self.data[i] += 1;
        return;
      }
    }
    // If we get here, the carry tried to propagate beyond the first byte of the
    // buffer, which should not be possible.
    panic!("Carry propagated too far in entropy encoder");
  }

  // Write an entropy-coded symbol using the given CDF
  // This does not yet implement CDF adaptation, so that must be turned off in the sequence header
  //
  // Note: Each CDF contains two implicit values:
  // * cdf[-1] = 0, so that when symbol == 0 "lo" is implicitly 0
  // * cdf[num_symbols - 1] = 32768, so that the probabilities sum to 1
  //
  // We do not store these values in the cdf array, and instead handle these cases
  // with ifs in this function
  pub fn write_symbol(&mut self, symbol: usize, cdf: &[u16]) {
    //println!("  Symbol({}, CDF = {:?})", symbol, cdf);
    let num_symbols = cdf.len() + 1;
    let inv_hi = if symbol == num_symbols - 1 { 0 } else { 32768 - (cdf[symbol] as u32) };

    // Update range to include new symbol
    if symbol == 0 {
      // inv_lo = 32768 implicitly
      self.range -= ((self.range >> 8) * (inv_hi >> 6) >> 1) + 4 * (num_symbols - 1) as u32;
    } else {
      let inv_lo = 32768 - (cdf[symbol - 1] as u32);

      let u = ((self.range >> 8) * (inv_lo >> 6) >> 1) + 4 * (num_symbols - symbol) as u32;
      let v = ((self.range >> 8) * (inv_hi >> 6) >> 1) + 4 * (num_symbols - symbol - 1) as u32;
      self.low += (self.range - u) as u64;
      self.range = u - v;
    }

    // Emit bytes if needed to normalize range
    let d = (15 - floor_log2(self.range)) as i32;
    let mut s = self.count + d;
    if s >= 40 {
      let num_bytes_ready = (s >> 3) + 1;
      let c = self.count + 24 - (num_bytes_ready << 3);

      let mut output = self.low >> c;
      self.low = self.low & ((1u64 << c) - 1);

      let carry = output & (1u64 << (num_bytes_ready << 3));
      output = output & ((1u64 << (num_bytes_ready << 3)) - 1);

      // Propagate carry backwards into existing data
      if carry != 0 {
        self.propagate_carry();
      }

      // Then append new bytes to output
      write_be_bytes(&mut self.data, output, num_bytes_ready as usize);

      s = c + d - 24;
    }

    self.low <<= d;
    self.range <<= d;
    self.count = s;
  }

  // Helper function: Write a single bit symbol, without needing extra syntax fluff to convert
  // from a single probability to a CDF
  // Note that, due to the way CDFs are encoded, the specified probability is the probability
  // of this bit being zero
  pub fn write_bit(&mut self, value: usize, p_zero: u16) {
    assert!(value == 0 || value == 1);
    self.write_symbol(value, &[p_zero]);
  }

  // Helper function: Write a flag which is logically a boolean
  // This is just syntactic sugar over self.write_bit(), mapping false => 0 and true => 1
  pub fn write_bool(&mut self, value: bool, p_false: u16) {
    self.write_symbol(value as usize, &[p_false]);
  }

  // Write an N-bit literal value. This means N bits, which are encoded
  // in high-to-low order with each bit having a 50:50 probability distribution
  pub fn write_literal(&mut self, value: u32, nbits: u32) {
    assert!(nbits <= 32);
    assert!(nbits == 32 || value < (1 << nbits));
    for shift in (0..nbits).rev() {
      let bit = (value >> shift) & 1;
      self.write_bit(bit as usize, 16384);
    }
  }

  // Encode a given value using a Golomb code
  pub fn write_golomb(&mut self, mut value: u32) {
    //println!("  Golomb({})", value);
    // Because the "standard" Golomb code cannot represent 0, we actually Golomb-code `value + 1`
    value += 1;

    let length = floor_log2(value);
    // Write `length` zero bits, then the full value, including the leading 1 bit
    // (which acts as a delimiter, allowing the decoder to figure out the correct length)
    self.write_literal(0, length);
    self.write_literal(value, length + 1);
  }

  // Finalize entropy block and return the generated bytes.
  // This takes care of two important requirements specified by AV1:
  // 1) The encoder must output enough extra bits to ensure that the decoder can
  //    unambiguously recover the correct value of all symbols
  // 2) There must be a trailing 1 bit at the end of each entropy coded block.
  //    Note that, if this is the last entropy coded block in a TILE_GROUP or FRAME OBU,
  //    then this also serves as the mandatory trailing 1 bit at the end of any OBU's content.
  pub fn finalize(mut self) -> Box<[u8]> {
    let mut s = self.count + 10;
    let m = 0x3FFF;
    
    // Inject a 1 bit in the right place
    let mut e = ((self.low + m) & !m) | (m + 1);
    let mut n = (1u64 << (self.count + 16)) - 1;

    // TODO: I think this can be simplified into a single round of
    // propagate_carry() + write_be_bytes(), but need to check that we won't overflow
    // any intermediate values
    while s > 0 {
      let val = e >> (self.count + 16);

      // Propagate carry backwards into existing data
      if (val & 0x100) != 0 {
        self.propagate_carry();
      }

      // Add new byte
      self.data.push((val & 0xFF) as u8);

      e = e & n;
      s -= 8;
      self.count -= 8;
      n >>= 8;
    }

    // Pull out and return entropy coded data, but drop the rest of `self`
    return self.data.into_boxed_slice();
  }
}
