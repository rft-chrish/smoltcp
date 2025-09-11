use crate::storage::RingBuffer;

/// A ring buffer with transmission masking capability.
/// 
/// This buffer allows marking specific bytes as "already transmitted" (e.g., by NIC hardware)
/// so they won't be transmitted again by the software stack, while still maintaining them
/// in the buffer for potential retransmission.
#[derive(Debug)]
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

    /// Get a slice of data that should actually be transmitted (applying the mask).
    /// 
    /// This filters out bytes that are marked as "already sent" and returns only
    /// the bytes that should be transmitted on the wire.
    pub fn get_transmittable(&self, offset: usize, size: usize) -> Vec<u8> {
        let data = self.buffer.get_allocated(offset, size);
        data.iter()
            .enumerate()
            .filter_map(|(i, &byte)| {
                let mask_idx = offset + i;
                if mask_idx < self.mask.len() && self.mask[mask_idx] {
                    Some(byte)
                } else {
                    None
                }
            })
            .collect()
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
        assert_eq!(transmittable, b"abcghi");
        
        // But all data should be readable
        let all_data = buffer.get_allocated(0, 9);
        assert_eq!(all_data, b"abcdefghi");
    }

    #[test]
    fn test_dequeue_updates_mask() {
        let mut buffer = MaskedSocketBuffer::new(RingBuffer::new(vec![0u8; 16]));
        
        // Setup: normal, already-sent, normal
        buffer.enqueue_slice(b"ab");      // positions 0-1: transmittable
        buffer.enqueue_already_sent(b"cd"); // positions 2-3: not transmittable
        buffer.enqueue_slice(b"ef");      // positions 4-5: transmittable
        
        // Check initial state
        let transmittable = buffer.get_transmittable(0, 6);
        assert_eq!(transmittable, b"abef");
        
        // Dequeue 3 bytes ("abc")
        let dequeued_slice = buffer.dequeue_many(3);
        assert_eq!(dequeued_slice, b"abc");
        assert_eq!(buffer.len(), 3);
        
        // Now positions should be shifted: "def" where "d" is not transmittable, "ef" are transmittable
        let transmittable = buffer.get_transmittable(0, 3);
        assert_eq!(transmittable, b"ef");
    }
}
