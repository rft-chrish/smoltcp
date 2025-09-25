use crate::storage::RingBuffer;
use core::fmt;

/// A ring buffer with transmission masking capability.
///
/// This buffer allows marking specific bytes as "already transmitted" (e.g., by NIC hardware)
/// so they won't be transmitted again by the software stack, while still maintaining them
/// in the buffer for potential retransmission.
pub struct MaskedSocketBuffer<'a> {
    buffer: RingBuffer<'a, u8>,
    /// Mask indicating which bytes should be transmitted.
    /// true = transmit normally, false = skip (already sent externally)
    mask: Vec<bool>,
}

impl<'a> MaskedSocketBuffer<'a> {
    /// Create a new masked socket buffer wrapping the given ring buffer.
    pub fn new(buffer: RingBuffer<'a, u8>) -> Self {
        let capacity = buffer.capacity();
        Self {
            buffer,
            mask: vec![true; capacity], // Default: all bytes should be transmitted
        }
    }

    /// Return the maximum number of elements that can be stored.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Query whether the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Query whether the buffer is full.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.buffer.is_full()
    }

    /// Return the current length of the buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Return the number of bytes that should be transmitted (where mask is true).
    pub fn len_to_transmit(&self) -> usize {
        let buffer_len = self.buffer.len();
        self.mask
            .iter()
            .take(buffer_len)
            .filter(|&&transmit| transmit)
            .count()
    }

    /// Return the number of elements that can be read.
    #[inline]
    pub fn window(&self) -> usize {
        self.buffer.window()
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
        // Reset mask to all true (transmittable)
        self.mask.fill(true);
    }

    /// Enqueue data that should be transmitted normally.
    ///
    /// Returns the number of elements actually enqueued.
    pub fn enqueue_slice(&mut self, data: &[u8]) -> usize {
        let start_pos = self.buffer.len();
        let size = self.buffer.enqueue_slice(data);

        // Mark new data as "should transmit"
        for i in start_pos..start_pos + size {
            if i < self.mask.len() {
                self.mask[i] = true;
            }
        }

        size
    }

    /// Enqueue data that has already been transmitted externally (e.g., by NIC).
    ///
    /// This data will be kept in the buffer for potential retransmission but will
    /// not be transmitted by the normal send path.
    ///
    /// Returns the number of elements actually enqueued.
    pub fn enqueue_already_sent(&mut self, data: &[u8]) -> usize {
        let start_pos = self.buffer.len();
        let size = self.buffer.enqueue_slice(data);

        // Mark new data as "do not transmit"
        for i in start_pos..start_pos + size {
            if i < self.mask.len() {
                self.mask[i] = false;
            }
        }

        size
    }

    /// Call `f` with the largest contiguous slice of octets that should be transmitted,
    /// and enqueue the amount of elements returned by `f`.
    ///
    /// This is the masked equivalent of RingBuffer::enqueue_many_with.
    pub fn enqueue_many_with<F, R>(&mut self, f: F) -> (usize, R)
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        let start_pos = self.buffer.len();
        let (size, result) = self.buffer.enqueue_many_with(f);

        // Mark new data as "should transmit"
        for i in start_pos..start_pos + size {
            if i < self.mask.len() {
                self.mask[i] = true;
            }
        }

        (size, result)
    }

    /// Get the next contiguous chunk of data that should be transmitted.
    ///
    /// Scans forward from offset to find the first contiguous sequence of transmittable bytes.
    /// Skips over non-transmittable bytes at the beginning, then returns the first contiguous
    /// transmittable chunk found within the requested range.
    /// This ensures proper packet boundaries for DMA/ethernet interface separation.
    pub fn get_transmittable(&self, offset: usize, size: usize) -> Vec<u8> {
        let data = self.buffer.get_allocated(offset, size);
        let mut result = Vec::new();
        let mut found_first_transmittable = false;

        for (i, &byte) in data.iter().enumerate() {
            let mask_idx = offset + i;

            // Check if this byte is transmittable
            if mask_idx < self.mask.len() && self.mask[mask_idx] {
                result.push(byte);
                found_first_transmittable = true;
            } else if found_first_transmittable {
                // Hit a non-transmittable byte after finding transmittable ones - stop here
                break;
            }
            // If we haven't found transmittable bytes yet, keep scanning
        }

        // Debug output
        if size > 0 {
            let mask_slice = if offset + size <= self.mask.len() {
                &self.mask[offset..offset + size]
            } else if offset < self.mask.len() {
                &self.mask[offset..]
            } else {
                &[]
            };
            println!(
                "get_transmittable: offset={}, size={}, buffer_len={}, mask_len={}",
                offset,
                size,
                self.buffer.len(),
                self.mask.len()
            );
            println!(
                "  data: {:?}",
                std::str::from_utf8(data).unwrap_or("<non-utf8>")
            );
            println!("  mask: {:?}", mask_slice);
            println!(
                "  result: {:?}",
                std::str::from_utf8(&result).unwrap_or("<non-utf8>")
            );
        }

        result
    }

    /// Find the next offset where transmittable data begins.
    ///
    /// Starting from `start_offset`, scans forward to find the first byte that is transmittable.
    /// Returns Some(offset) if found within the buffer, or None if no transmittable bytes remain.
    pub fn find_next_transmittable_offset(&self, start_offset: usize) -> Option<usize> {
        let buffer_len = self.buffer.len();

        for offset in start_offset..buffer_len {
            if offset < self.mask.len() && self.mask[offset] {
                return Some(offset);
            }
        }

        None
    }

    /// Check if any bytes in the given range are masked (not transmittable).
    ///
    /// Returns true if all bytes in the range should be transmitted (no masking).
    pub fn is_range_fully_transmittable(&self, offset: usize, size: usize) -> bool {
        let range_end = offset + size;
        if range_end > self.buffer.len() {
            return false;
        }

        self.mask[offset..range_end]
            .iter()
            .all(|&transmit| transmit)
    }

    /// Get an allocated slice from the buffer (ignoring mask).
    ///
    /// This is useful for operations that need to see all data regardless of
    /// transmission status.
    pub fn get_allocated(&self, offset: usize, size: usize) -> &[u8] {
        self.buffer.get_allocated(offset, size)
    }

    /// Dequeue elements and update the mask accordingly.
    ///
    /// Returns the dequeued slice.
    pub fn dequeue_many(&mut self, count: usize) -> &mut [u8] {
        let capacity = self.buffer.capacity();
        let dequeued_slice = self.buffer.dequeue_many(count);
        let dequeued_count = dequeued_slice.len();

        if dequeued_count > 0 {
            // Shift mask to remove dequeued elements
            self.mask.drain(0..dequeued_count);
            // Pad with true values at the end
            self.mask.resize(capacity, true);
        }

        dequeued_slice
    }

    /// Dequeue enough data to get the specified number of transmittable bytes.
    ///
    /// Returns (transmittable_bytes, untransmittable_bytes) where:
    /// - transmittable_bytes: Vec containing only the transmittable bytes
    /// - untransmittable_bytes: Vec containing any masked bytes that were dequeued
    ///
    /// This method will dequeue as many bytes as necessary from the start of the buffer
    /// to collect `transmittable_count` transmittable bytes.
    pub fn dequeue_transmittable(&mut self, transmittable_count: usize) -> (Vec<u8>, Vec<u8>) {
        let mut transmittable_bytes = Vec::new();
        let mut untransmittable_bytes = Vec::new();
        let mut total_to_dequeue = 0;

        // Scan through buffer to find how many bytes we need to dequeue
        let buffer_len = self.buffer.len();
        for i in 0..buffer_len {
            if transmittable_bytes.len() >= transmittable_count {
                break;
            }

            let data_byte = self.buffer.get_allocated(i, 1)[0];
            total_to_dequeue += 1;

            if i < self.mask.len() && self.mask[i] {
                // Transmittable byte
                transmittable_bytes.push(data_byte);
            } else {
                // Masked/untransmittable byte
                untransmittable_bytes.push(data_byte);
            }
        }

        // Actually dequeue the bytes from the buffer
        if total_to_dequeue > 0 {
            let _ = self.dequeue_many(total_to_dequeue);
        }

        (transmittable_bytes, untransmittable_bytes)
    }

    /// Call `f` with the largest contiguous slice of octets and dequeue
    /// the amount of elements returned by `f`.
    ///
    /// This operates on all data regardless of mask status.
    pub fn dequeue_many_with<'b, F, R>(&'b mut self, f: F) -> (usize, R)
    where
        F: FnOnce(&'b mut [u8]) -> (usize, R),
    {
        let capacity = self.buffer.capacity();
        let (dequeued_count, result) = self.buffer.dequeue_many_with(f);

        if dequeued_count > 0 {
            // Shift mask to remove dequeued elements
            self.mask.drain(0..dequeued_count);
            // Pad with true values at the end
            self.mask.resize(capacity, true);
        }

        (dequeued_count, result)
    }

    /// Dequeue a slice of elements.
    ///
    /// Returns the number of elements actually dequeued.
    pub fn dequeue_slice(&mut self, data: &mut [u8]) -> usize {
        let dequeued = self.buffer.dequeue_slice(data);

        if dequeued > 0 {
            // Shift mask to remove dequeued elements
            self.mask.drain(0..dequeued);
            // Pad with true values at the end
            self.mask.resize(self.buffer.capacity(), true);
        }

        dequeued
    }

    /// Read data from the buffer without dequeuing.
    ///
    /// Returns the number of elements actually read.
    pub fn read_allocated(&mut self, offset: usize, data: &mut [u8]) -> usize {
        self.buffer.read_allocated(offset, data)
    }

    /// Dequeue a given number of elements from the buffer.
    pub fn dequeue_allocated(&mut self, count: usize) {
        if count > 0 {
            let capacity = self.buffer.capacity();
            self.buffer.dequeue_allocated(count);

            // Shift mask to remove dequeued elements
            self.mask.drain(0..count);
            // Pad with true values at the end
            self.mask.resize(capacity, true);
        }
    }

    /// Write data to unallocated space at the given offset.
    ///
    /// Returns the number of bytes actually written.
    pub fn write_unallocated(&mut self, offset: usize, data: &[u8]) -> usize {
        let written = self.buffer.write_unallocated(offset, data);
        // No need to update mask since this is writing to unallocated space
        written
    }

    /// Enqueue unallocated space in the buffer.
    pub fn enqueue_unallocated(&mut self, count: usize) {
        let start_pos = self.buffer.len();
        self.buffer.enqueue_unallocated(count);

        // Mark new data as "should transmit" (default behavior)
        for i in start_pos..start_pos + count {
            if i < self.mask.len() {
                self.mask[i] = true;
            }
        }
    }

    /// Get a slice of data from the buffer (ignoring mask) as a Vec.
    ///
    /// This is useful when you need the actual data regardless of mask state.
    /// Handles ring buffer wrap-around by collecting data from multiple contiguous segments.
    pub fn get_unmasked(&self, offset: usize, end: usize) -> Vec<u8> {
        let size = end - offset;
        if size == 0 {
            return Vec::new();
        }

        // Get the first contiguous segment
        let first_segment = self.buffer.get_allocated(offset, size);
        let first_len = first_segment.len();

        // If we got all the data in the first segment, we're done
        if first_len == size {
            return first_segment.to_vec();
        }

        // Otherwise, we need to handle wrap-around
        let mut result = Vec::with_capacity(size);
        result.extend_from_slice(first_segment);

        // Calculate how much more data we need from the wrapped portion
        let remaining = size - first_len;
        let second_offset = offset + first_len;
        let second_segment = self.buffer.get_allocated(second_offset, remaining);
        result.extend_from_slice(second_segment);

        result
    }

    /// Mark a range of already-enqueued data as "already sent".
    ///
    /// This is used when data in the buffer has been sent externally (e.g., by FPGA).
    /// The offset and size are logical positions in the buffer.
    pub fn mark_as_already_sent(&mut self, offset: usize, size: usize) {
        let range_end = offset + size;
        for i in offset..range_end {
            if i < self.buffer.len() {
                self.mask[i] = false;
            }
        }
    }

    /// Reset the mask for a range of data to mark it as transmittable again.
    ///
    /// This is used during retransmission to ensure previously sent data can be
    /// retransmitted. The offset and size are logical positions in the buffer.
    pub fn reset_mask_range(&mut self, offset: usize, size: usize) {
        let range_end = offset + size;
        for i in offset..range_end {
            if i < self.buffer.len() && i < self.mask.len() {
                self.mask[i] = true;
            }
        }
    }

    /// Get debug information about the mask state.
    #[cfg(any(test, feature = "verbose"))]
    pub fn mask_debug_info(&self) -> (usize, usize) {
        let total = self.len();
        let transmittable = self.mask.iter().take(total).filter(|&&x| x).count();
        (transmittable, total)
    }
}

impl<'a> From<RingBuffer<'a, u8>> for MaskedSocketBuffer<'a> {
    fn from(buffer: RingBuffer<'a, u8>) -> Self {
        Self::new(buffer)
    }
}

impl<'a> fmt::Debug for MaskedSocketBuffer<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buffer_len = self.buffer.len();

        if buffer_len == 0 {
            return write!(f, "MaskedSocketBuffer {{ empty }}");
        }

        // Build debug representation showing data and mask status
        let mut content = String::with_capacity(buffer_len * 4);

        // Get data byte by byte to handle wraparound properly
        for i in 0..buffer_len {
            if i > 0 {
                content.push(' ');
            }

            // Get a single byte at position i
            let byte_slice = self.buffer.get_allocated(i, 1);
            if byte_slice.is_empty() {
                // This shouldn't happen if buffer_len is correct
                content.push_str("??");
                continue;
            }
            let byte = byte_slice[0];

            let is_transmittable = i < self.mask.len() && self.mask[i];

            if is_transmittable {
                // Transmittable bytes shown normally
                if byte.is_ascii_graphic() || byte == b' ' {
                    content.push_str(&format!("'{}'", byte as char));
                } else {
                    content.push_str(&format!("{:02x}", byte));
                }
            } else {
                // Masked bytes shown with brackets
                if byte.is_ascii_graphic() || byte == b' ' {
                    content.push_str(&format!("['{}'']", byte as char));
                } else {
                    content.push_str(&format!("[{:02x}]", byte));
                }
            }
        }

        let (transmittable_count, total_count) = (self.len_to_transmit(), buffer_len);

        write!(
            f,
            "MaskedSocketBuffer {{ {}/{} transmittable: {} }}",
            transmittable_count, total_count, content
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_enqueue_dequeue() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Enqueue normal data
        let size = buffer.enqueue_slice(b"hello");
        assert_eq!(size, 5);
        assert_eq!(buffer.len(), 5);

        // All should be transmittable
        let transmittable = buffer.get_transmittable(0, 5);
        assert_eq!(transmittable, b"hello");

        // Dequeue
        let mut data = [0u8; 5];
        let dequeued = buffer.dequeue_slice(&mut data);
        assert_eq!(dequeued, 5);
        assert_eq!(&data, b"hello");
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn test_already_sent_masking() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Enqueue normal data
        buffer.enqueue_slice(b"abc");

        // Enqueue already-sent data
        buffer.enqueue_already_sent(b"def");

        // Enqueue more normal data
        buffer.enqueue_slice(b"ghi");

        assert_eq!(buffer.len(), 9);

        // Only "abc" and "ghi" should be transmittable
        let transmittable = buffer.get_transmittable(0, 9);
        assert_eq!(transmittable, b"abc");

        // But all data should be readable
        let all_data = buffer.get_allocated(0, 9);
        assert_eq!(all_data, b"abcdefghi");
    }

    #[test]
    fn test_dequeue_updates_mask() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Setup: normal, already-sent, normal
        buffer.enqueue_slice(b"ab"); // positions 0-1: transmittable
        buffer.enqueue_already_sent(b"cd"); // positions 2-3: not transmittable
        buffer.enqueue_slice(b"ef"); // positions 4-5: transmittable

        // Check initial state
        let transmittable = buffer.get_transmittable(0, 6);
        assert_eq!(transmittable, b"ab");

        // Dequeue 3 bytes ("abc")
        let dequeued_slice = buffer.dequeue_many(3);
        assert_eq!(dequeued_slice, b"abc");
        assert_eq!(buffer.len(), 3);

        // Now positions should be shifted: "def" where "d" is not transmittable, "ef" are transmittable
        let transmittable = buffer.get_transmittable(0, 3);
        assert_eq!(transmittable, b"ef");
    }

    #[test]
    fn test_dequeue_transmittable() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Setup buffer: exxxyyy where "e" is masked (from WrongSynchronizer simulation)
        buffer.enqueue_already_sent(b"e"); // masked byte first
        buffer.enqueue_slice(b"xxxyyy"); // normal bytes

        // Verify buffer state
        assert_eq!(buffer.len(), 7);
        assert_eq!(buffer.len_to_transmit(), 6); // only "xxxyyy" should be transmittable

        // Show debug output
        println!("Buffer state: {:?}", buffer);

        // Dequeue 3 transmittable bytes
        let (transmittable, masked) = buffer.dequeue_transmittable(3);
        assert_eq!(transmittable, b"xxx");
        assert_eq!(masked, b"e");

        // Buffer should now have "yyy" remaining
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.len_to_transmit(), 3);

        println!("After dequeue: {:?}", buffer);

        // Get remaining transmittable data
        let remaining = buffer.get_transmittable(0, 3);
        assert_eq!(remaining, b"yyy");
    }

    #[test]
    fn test_debug_output() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Test empty buffer
        println!("Empty: {:?}", buffer);

        // Test mixed content
        buffer.enqueue_slice(b"ABC"); // transmittable
        buffer.enqueue_already_sent(b"xy"); // masked
        buffer.enqueue_slice(b"123"); // transmittable

        println!("Mixed: {:?}", buffer);

        // Test with non-ASCII bytes
        buffer.enqueue_already_sent(&[0xFF, 0x00]); // masked non-ASCII
        buffer.enqueue_slice(&[0x0A, 0x20]); // transmittable non-ASCII

        println!("With non-ASCII: {:?}", buffer);
    }

    #[test]
    fn test_debug_with_wraparound() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 8]));

        // Fill buffer with some data
        buffer.enqueue_slice(b"abcdef"); // 6 bytes

        // Dequeue some to create wraparound scenario
        let _ = buffer.dequeue_many(4); // Remove "abcd", leaves "ef"

        // Add more data that will wrap around
        buffer.enqueue_already_sent(b"X"); // masked
        buffer.enqueue_slice(b"12345"); // will wrap around

        println!("Wraparound buffer: {:?}", buffer);

        // Verify the debug output shows all data
        let debug_str = format!("{:?}", buffer);
        assert!(debug_str.contains("'e'"));
        assert!(debug_str.contains("'f'"));
        assert!(debug_str.contains("['X'']"));
        assert!(debug_str.contains("'1'"));
        assert!(debug_str.contains("'2'"));
        assert!(debug_str.contains("'3'"));
        assert!(debug_str.contains("'4'"));
        assert!(debug_str.contains("'5'"));
    }

    #[test]
    fn test_get_unmasked_wraparound() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 8]));

        // Fill buffer to near capacity
        buffer.enqueue_slice(b"abcdef"); // 6 bytes

        // Dequeue some to create space at beginning and force wraparound
        let _ = buffer.dequeue_many(4); // Remove "abcd", leaves "ef" at positions 0-1

        // Add more data that will wrap around
        buffer.enqueue_slice(b"ghijk"); // 5 bytes, will wrap around

        // Buffer now has "ef" followed by "ghijk" that wraps around
        // Verify we can get all the data correctly
        let all_data = buffer.get_unmasked(0, buffer.len());
        assert_eq!(all_data, b"efghijk");

        // Test getting partial data that crosses wrap boundary
        let partial = buffer.get_unmasked(1, 5); // Should get "fghi"
        assert_eq!(partial, b"fghi");

        // Test getting data from end that wraps
        let end_data = buffer.get_unmasked(2, 7); // Should get "ghijk"
        assert_eq!(end_data, b"ghijk");
    }

    #[test]
    fn test_reset_mask_range() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Add some data
        buffer.enqueue_slice(b"abcdefghij");
        assert_eq!(buffer.len(), 10);

        // Mark some bytes as already sent
        buffer.mark_as_already_sent(2, 4); // Mark "cdef" as already sent

        // Verify mask state
        assert_eq!(buffer.get_transmittable(0, 10), b"ab");
        assert!(!buffer.is_range_fully_transmittable(0, 10));
        assert!(buffer.is_range_fully_transmittable(0, 2)); // "ab"
        assert!(!buffer.is_range_fully_transmittable(2, 4)); // "cdef"
        assert!(buffer.is_range_fully_transmittable(6, 4)); // "ghij"

        // Reset mask for the masked range
        buffer.reset_mask_range(2, 4);

        // Verify all data is now transmittable again
        assert_eq!(buffer.get_transmittable(0, 10), b"abcdefghij");
        assert!(buffer.is_range_fully_transmittable(0, 10));
        assert!(buffer.is_range_fully_transmittable(2, 4)); // "cdef" should be transmittable again

        // Test partial reset
        buffer.mark_as_already_sent(1, 8); // Mark "bcdefghi" as already sent
        assert_eq!(buffer.get_transmittable(0, 10), b"a");

        // Reset only middle portion
        buffer.reset_mask_range(3, 4); // Reset "defg"
        assert_eq!(buffer.get_transmittable(0, 10), b"a");

        // Test reset beyond buffer length (should not crash)
        buffer.reset_mask_range(0, 20); // Beyond buffer size
        assert_eq!(buffer.get_transmittable(0, 10), b"abcdefghij");
    }

    #[test]
    fn test_get_transmittable_contiguous_chunks() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));

        // Test case 1: a,b,c,d,e with mask T,T,T,F,T
        buffer.enqueue_slice(b"abcde");
        buffer.mark_as_already_sent(3, 1); // Mark 'd' as already sent (mask = false)

        let chunk1 = buffer.get_transmittable(0, 5);
        assert_eq!(chunk1, b"abc"); // Should get first contiguous chunk "abc", stop at 'd'

        // Test case 2: Starting from a non-transmittable byte should skip to next chunk
        let chunk2 = buffer.get_transmittable(3, 2);
        assert_eq!(chunk2, b"e"); // Should skip 'd' and get 'e'

        // Clear buffer and test case 3: a,b,c,d,e with mask F,T,T,F,T
        buffer.clear();
        buffer.enqueue_slice(b"abcde");
        buffer.mark_as_already_sent(0, 1); // Mark 'a' as already sent
        buffer.mark_as_already_sent(3, 1); // Mark 'd' as already sent

        let chunk3 = buffer.get_transmittable(0, 5);
        assert_eq!(chunk3, b"bc"); // Should skip 'a', get "bc", stop at 'd'

        // Test case 4: Starting from middle should still work
        let chunk4 = buffer.get_transmittable(3, 2);
        assert_eq!(chunk4, b"e"); // Should skip 'd' and get 'e'

        // Test case 5: All bytes masked
        buffer.mark_as_already_sent(1, 2); // Mark 'b','c' as already sent
        buffer.mark_as_already_sent(4, 1); // Mark 'e' as already sent
        let chunk5 = buffer.get_transmittable(0, 5);
        assert_eq!(chunk5, b""); // Should return empty - no transmittable bytes

        // Test case 6: Original example - verify it works as expected
        buffer.clear();
        buffer.enqueue_slice(b"abcdefgh");
        // Create mask pattern: T,T,T,F,F,T,T,F
        buffer.mark_as_already_sent(3, 2); // Mark 'd','e' as already sent
        buffer.mark_as_already_sent(7, 1); // Mark 'h' as already sent

        let chunk6 = buffer.get_transmittable(0, 8);
        assert_eq!(chunk6, b"abc"); // First chunk

        let chunk7 = buffer.get_transmittable(3, 5);
        assert_eq!(chunk7, b"fg"); // Should skip 'd','e' and get "fg"
    }
}
